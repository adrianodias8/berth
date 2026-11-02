import AppKit
import Carbon.HIToolbox
import Metal
import QuartzCore

// A live surface on screen: a CAMetalLayer driven by the view's display
// link (macOS 14's NSView.displayLink, so frames are paced to the display
// the window is on, at 60 or 120 Hz). On a tick with a newer doorbell the
// slot is blitted straight out of the mapped ring into a private texture;
// when the blit completes the slot's seqlock is read again, and a frame the
// daemon overwrote meanwhile is dropped (the next doorbell is already on
// its way). Then the texture is drawn aspect-fit with the guest's cursor
// on top. Ticks with nothing new do nothing, so a static screen costs ~0.

final class MetalFrameView: NSView {
    /// The surface shown; nil shows black.
    var stream: SurfaceStream? {
        didSet {
            guard stream !== oldValue else { return }
            drawnSerial = .max
            front = nil
            needsPresent = true
        }
    }

    var paused = false {
        didSet {
            if !paused { needsPresent = true }
            updateLinkPaused()
        }
    }

    /// Whether Mac input goes to the guest. Set it through `setControlling`.
    private(set) var controlling = false
    /// Control changed from inside the view (Ctrl-Opt-Return).
    var onControlChange: ((Bool) -> Void)?
    /// Frames shown in the last second, reported once a second.
    var onFPS: ((Int) -> Void)?

    private let device = MTLCreateSystemDefaultDevice()
    private lazy var queue = device?.makeCommandQueue()
    private var framePipeline: MTLRenderPipelineState?
    private var cursorPipeline: MTLRenderPipelineState?
    private var nearest: MTLSamplerState?
    private var linear: MTLSamplerState?
    private var metalLayer: CAMetalLayer { layer as! CAMetalLayer }
    private var link: CADisplayLink?
    private var occlusion: NSObjectProtocol?

    /// The ring `buffer` wraps, held so the mapping outlives the buffer.
    private var bufferRing: Ring?
    private var buffer: MTLBuffer?
    private var front: MTLTexture?
    private var back: MTLTexture?
    private var cursorTexture: MTLTexture?
    private var cursorUploaded: UInt64 = .max
    private var cursorShown: RingCursor?
    /// The `Pending.serial` the front texture holds.
    private var drawnSerial: UInt64 = .max
    private var blitInFlight = false
    private var needsPresent = true

    private var framesThisSecond = 0
    private var secondStarted = CACurrentMediaTime()
    private var lastFPS = -1

    private var modifiers = ModifierTracker()
    private var scroll = ScrollAccumulator()
    private var tracking: NSTrackingArea?

    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
        layerContentsRedrawPolicy = .never
        setUpPipelines()
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override func makeBackingLayer() -> CALayer {
        let layer = CAMetalLayer()
        layer.device = device
        layer.pixelFormat = .bgra8Unorm
        layer.framebufferOnly = true
        layer.maximumDrawableCount = 3
        layer.isOpaque = true
        layer.backgroundColor = NSColor.black.cgColor
        return layer
    }

    // MARK: Display link

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        link?.invalidate()
        link = nil
        if let occlusion { NotificationCenter.default.removeObserver(occlusion) }
        occlusion = nil
        guard window != nil else {
            if controlling { setControlling(false) }
            return
        }
        updateDrawableSize()
        // The link holds its target: invalidated when the view leaves its
        // window, above.
        let link = displayLink(target: self, selector: #selector(tick(_:)))
        link.add(to: .main, forMode: .common)
        self.link = link
        occlusion = NotificationCenter.default.addObserver(
            forName: NSWindow.didChangeOcclusionStateNotification, object: window, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.updateLinkPaused() }
        }
        updateLinkPaused()
    }

    /// No ticks at all while paused or hidden, rather than ticks that
    /// return early.
    private func updateLinkPaused() {
        let visible = window?.occlusionState.contains(.visible) ?? false
        link?.isPaused = paused || !visible
        if !(link?.isPaused ?? true) { needsPresent = true }
    }

    deinit {
        if let occlusion { NotificationCenter.default.removeObserver(occlusion) }
    }

    override func viewDidChangeBackingProperties() {
        super.viewDidChangeBackingProperties()
        updateDrawableSize()
    }

    override func setFrameSize(_ newSize: NSSize) {
        super.setFrameSize(newSize)
        updateDrawableSize()
    }

    private func updateDrawableSize() {
        let scale = window?.backingScaleFactor ?? 2
        metalLayer.contentsScale = scale
        let size = CGSize(width: max(1, bounds.width * scale), height: max(1, bounds.height * scale))
        if metalLayer.drawableSize != size { metalLayer.drawableSize = size }
        needsPresent = true
    }

    @objc private func tick(_ link: CADisplayLink) {
        countFPS()
        guard !paused, let stream, window?.occlusionState.contains(.visible) ?? false else { return }
        let (ring, pending) = stream.current
        guard let ring else { return }
        if pending.serial != drawnSerial, !blitInFlight { startBlit(ring, pending) }
        let cursor = ring.cursor
        if cursor != cursorShown { needsPresent = true }
        if needsPresent, front != nil, !blitInFlight { present(ring, cursor) }
    }

    private func countFPS() {
        let now = CACurrentMediaTime()
        guard now - secondStarted >= 1 else { return }
        let fps = Int((Double(framesThisSecond) / (now - secondStarted)).rounded())
        framesThisSecond = 0
        secondStarted = now
        if fps != lastFPS {
            lastFPS = fps
            onFPS?(fps)
        }
    }

    // MARK: Frames

    private func startBlit(_ ring: Ring, _ pending: SurfaceStream.Pending) {
        guard let device, let queue else { return }
        if bufferRing !== ring || buffer == nil {
            buffer = ring.makeBuffer(device)
            bufferRing = ring
        }
        guard let buffer else { return }
        if back == nil || back!.width != ring.width || back!.height != ring.height {
            back = Self.privateTexture(device, width: ring.width, height: ring.height)
        }
        guard let target = back, let commands = queue.makeCommandBuffer(),
              let read = Self.encodeSlotRead(commands, buffer: buffer, ring: ring, slot: pending.slot, into: target)
        else { return } // the slot is being written: try again next tick
        blitInFlight = true
        let serial = pending.serial
        // `read` holds the ring, and with it the mapping under `buffer`,
        // until the GPU is done with it.
        commands.addCompletedHandler { [weak self] _ in
            let intact = read.intact
            DispatchQueue.main.async { self?.blitDone(intact: intact, serial: serial) }
        }
        commands.commit()
    }

    /// A slot copied out of the ring, and the seqlock word it was copied
    /// under. Once the GPU has read it, `intact` says whether the daemon
    /// left the slot alone meanwhile.
    struct SlotRead {
        let ring: Ring
        let slot: Int
        let seq: UInt64

        var intact: Bool { ring.slotSeq(slot) == seq }
    }

    /// Encodes a copy of one slot of the mapped ring into `texture`, rows
    /// `stride` apart; nil when the slot is mid-write (its word is odd).
    static func encodeSlotRead(_ commands: MTLCommandBuffer, buffer: MTLBuffer, ring: Ring, slot: Int,
                               into texture: MTLTexture) -> SlotRead? {
        let seq = ring.slotSeq(slot)
        guard seq != 0, seq & 1 == 0, let blit = commands.makeBlitCommandEncoder() else { return nil }
        blit.copy(from: buffer, sourceOffset: ring.slotOffsets[slot], sourceBytesPerRow: ring.stride,
                  sourceBytesPerImage: ring.stride * ring.height,
                  sourceSize: MTLSize(width: ring.width, height: ring.height, depth: 1),
                  to: texture, destinationSlice: 0, destinationLevel: 0, destinationOrigin: MTLOrigin(x: 0, y: 0, z: 0))
        blit.endEncoding()
        return SlotRead(ring: ring, slot: slot, seq: seq)
    }

    private func blitDone(intact: Bool, serial: UInt64) {
        blitInFlight = false
        // Torn: the slot was rewritten under the blit. Keep the old frame.
        guard intact else { return }
        swap(&front, &back)
        drawnSerial = serial
        needsPresent = true
        framesThisSecond += 1
    }

    private func present(_ ring: Ring, _ cursor: RingCursor) {
        guard let device, let queue, let front, let framePipeline,
              let drawable = metalLayer.nextDrawable(), let commands = queue.makeCommandBuffer() else { return }
        needsPresent = false
        cursorShown = cursor
        if cursor.visible, cursor.width > 0, cursor.height > 0, cursor.seq != cursorUploaded,
           let buffer, bufferRing === ring {
            if cursorTexture == nil {
                cursorTexture = Self.privateTexture(device, width: RingLayout.cursorSide, height: RingLayout.cursorSide)
            }
            if let cursorTexture, let blit = commands.makeBlitCommandEncoder() {
                blit.copy(from: buffer, sourceOffset: ring.cursorOffset, sourceBytesPerRow: RingLayout.cursorSide * 4,
                          sourceBytesPerImage: RingLayout.cursorBytes,
                          sourceSize: MTLSize(width: RingLayout.cursorSide, height: RingLayout.cursorSide, depth: 1),
                          to: cursorTexture, destinationSlice: 0, destinationLevel: 0,
                          destinationOrigin: MTLOrigin(x: 0, y: 0, z: 0))
                blit.endEncoding()
                cursorUploaded = cursor.seq
                // The copy reads the ring's mapping, which `buffer` doesn't
                // own: a resize can drop this Ring before the GPU is done.
                commands.addCompletedHandler { _ in withExtendedLifetime(ring) {} }
            }
        }
        let pass = MTLRenderPassDescriptor()
        pass.colorAttachments[0].texture = drawable.texture
        pass.colorAttachments[0].loadAction = .clear
        pass.colorAttachments[0].storeAction = .store
        pass.colorAttachments[0].clearColor = MTLClearColor(red: 0, green: 0, blue: 0, alpha: 1)
        guard let encoder = commands.makeRenderCommandEncoder(descriptor: pass) else { return }
        let drawableSize = CGSize(width: drawable.texture.width, height: drawable.texture.height)
        let guest = CGSize(width: front.width, height: front.height)
        let fit = GuestGeometry.fit(guest: guest, in: drawableSize)
        let scale = fit.width / guest.width
        // Pixel-exact at whole-number scales; smooth otherwise.
        let whole = abs(scale - scale.rounded()) < 0.001 && scale >= 1
        encoder.setRenderPipelineState(framePipeline)
        var quad = Self.quad(fit, in: drawableSize, uv: SIMD2(1, 1))
        encoder.setVertexBytes(&quad, length: MemoryLayout<Quad>.stride, index: 0)
        encoder.setFragmentTexture(front, index: 0)
        encoder.setFragmentSamplerState(whole ? nearest : linear, index: 0)
        encoder.drawPrimitives(type: .triangleStrip, vertexStart: 0, vertexCount: 4)
        if cursor.visible, cursorUploaded == cursor.seq, let cursorTexture, let cursorPipeline {
            let rect = CGRect(x: fit.minX + CGFloat(cursor.x - cursor.hotX) * scale,
                              y: fit.minY + CGFloat(cursor.y - cursor.hotY) * scale,
                              width: CGFloat(cursor.width) * scale, height: CGFloat(cursor.height) * scale)
            let side = Float(RingLayout.cursorSide)
            var quad = Self.quad(rect, in: drawableSize, uv: SIMD2(Float(cursor.width) / side, Float(cursor.height) / side))
            encoder.setRenderPipelineState(cursorPipeline)
            encoder.setVertexBytes(&quad, length: MemoryLayout<Quad>.stride, index: 0)
            encoder.setFragmentTexture(cursorTexture, index: 0)
            encoder.setFragmentSamplerState(whole ? nearest : linear, index: 0)
            encoder.drawPrimitives(type: .triangleStrip, vertexStart: 0, vertexCount: 4)
        }
        encoder.endEncoding()
        commands.present(drawable)
        commands.commit()
    }

    // MARK: Metal setup

    /// A rect in pixels from the top-left, as the shader's NDC corners.
    struct Quad {
        var rect: SIMD4<Float>
        var uvMax: SIMD2<Float>
    }

    private static func quad(_ rect: CGRect, in size: CGSize, uv: SIMD2<Float>) -> Quad {
        func x(_ v: CGFloat) -> Float { Float(v / size.width * 2 - 1) }
        func y(_ v: CGFloat) -> Float { Float(1 - v / size.height * 2) }
        return Quad(rect: SIMD4(x(rect.minX), y(rect.minY), x(rect.maxX), y(rect.maxY)), uvMax: uv)
    }

    static func privateTexture(_ device: MTLDevice, width: Int, height: Int) -> MTLTexture? {
        let desc = MTLTextureDescriptor.texture2DDescriptor(pixelFormat: .bgra8Unorm, width: width, height: height, mipmapped: false)
        desc.storageMode = .private
        desc.usage = .shaderRead
        return device.makeTexture(descriptor: desc)
    }

    /// Compiled at run time, so building the app needs no Metal toolchain.
    static let shaders = """
    #include <metal_stdlib>
    using namespace metal;

    struct Quad { float4 rect; float2 uvMax; };
    struct VOut { float4 pos [[position]]; float2 uv; };

    vertex VOut quad_vs(uint vid [[vertex_id]], constant Quad &q [[buffer(0)]]) {
        float2 c = float2(vid & 1, vid >> 1);
        VOut o;
        o.pos = float4(mix(q.rect.x, q.rect.z, c.x), mix(q.rect.y, q.rect.w, c.y), 0, 1);
        o.uv = c * q.uvMax;
        return o;
    }

    // The guest's XRGB leaves alpha undefined: the frame is opaque.
    fragment float4 frame_fs(VOut in [[stage_in]], texture2d<float> t [[texture(0)]], sampler s [[sampler(0)]]) {
        return float4(t.sample(s, in.uv).rgb, 1);
    }

    fragment float4 cursor_fs(VOut in [[stage_in]], texture2d<float> t [[texture(0)]], sampler s [[sampler(0)]]) {
        return t.sample(s, in.uv);
    }
    """

    private func setUpPipelines() {
        guard let device else { return }
        do {
            let library = try device.makeLibrary(source: Self.shaders, options: nil)
            let desc = MTLRenderPipelineDescriptor()
            desc.vertexFunction = library.makeFunction(name: "quad_vs")
            desc.fragmentFunction = library.makeFunction(name: "frame_fs")
            desc.colorAttachments[0].pixelFormat = .bgra8Unorm
            framePipeline = try device.makeRenderPipelineState(descriptor: desc)
            // Cursor images are premultiplied ARGB (virtio-gpu, wl_shm).
            desc.fragmentFunction = library.makeFunction(name: "cursor_fs")
            let blend = desc.colorAttachments[0]!
            blend.isBlendingEnabled = true
            blend.sourceRGBBlendFactor = .one
            blend.sourceAlphaBlendFactor = .one
            blend.destinationRGBBlendFactor = .oneMinusSourceAlpha
            blend.destinationAlphaBlendFactor = .oneMinusSourceAlpha
            cursorPipeline = try device.makeRenderPipelineState(descriptor: desc)
        } catch {
            NSLog("berth: display shaders: \(error)")
        }
        nearest = Self.sampler(device, .nearest)
        linear = Self.sampler(device, .linear)
    }

    private static func sampler(_ device: MTLDevice, _ filter: MTLSamplerMinMagFilter) -> MTLSamplerState? {
        let desc = MTLSamplerDescriptor()
        desc.minFilter = filter
        desc.magFilter = filter
        desc.sAddressMode = .clampToEdge
        desc.tAddressMode = .clampToEdge
        return device.makeSamplerState(descriptor: desc)
    }

    // MARK: Take control

    /// Starts or stops sending Mac input to the guest. Stopping releases
    /// whatever is held first, while the daemon still forwards it.
    func setControlling(_ on: Bool) {
        guard on != controlling else { return }
        if on {
            controlling = true
            stream?.send(.control(on: true))
            window?.makeFirstResponder(self)
        } else {
            stream?.send(.releaseAll)
            stream?.send(.control(on: false))
            controlling = false
            modifiers.reset()
            scroll.reset()
        }
    }

    private func send(_ event: InputEvent) {
        guard controlling else { return }
        stream?.send(event)
    }

    /// Releases held keys and buttons, keeping control: the window lost
    /// focus, and their key-ups will go to someone else.
    private func releaseHeld() {
        guard controlling else { return }
        stream?.send(.releaseAll)
        modifiers.reset()
    }

    override var acceptsFirstResponder: Bool { true }

    override func resignFirstResponder() -> Bool {
        releaseHeld()
        return super.resignFirstResponder()
    }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if let tracking { removeTrackingArea(tracking) }
        let area = NSTrackingArea(rect: .zero, options: [.mouseMoved, .activeInKeyWindow, .inVisibleRect], owner: self)
        addTrackingArea(area)
        tracking = area
    }

    /// The keys that stay the Mac's while in control.
    private static func passesThrough(_ event: NSEvent) -> Bool {
        let flags = event.modifierFlags.intersection(.deviceIndependentFlagsMask)
        return flags.contains(.command) && [kVK_ANSI_Q, kVK_ANSI_W].contains(Int(event.keyCode))
    }

    private static func isReleaseChord(_ event: NSEvent) -> Bool {
        let flags = event.modifierFlags.intersection(.deviceIndependentFlagsMask)
        return flags.contains([.control, .option])
            && [kVK_Return, kVK_ANSI_KeypadEnter].contains(Int(event.keyCode))
    }

    override func performKeyEquivalent(with event: NSEvent) -> Bool {
        guard controlling, window?.firstResponder === self, event.type == .keyDown,
              event.modifierFlags.contains(.command), !Self.passesThrough(event) else {
            return super.performKeyEquivalent(with: event)
        }
        // Command shortcuts are the guest's (Super+key); the app's menus
        // never see them. Their key-up never arrives, so press and release.
        if !event.isARepeat, let code = KeyMap.evdev(macKeyCode: Int(event.keyCode)) {
            send(.key(code: code, down: true))
            send(.key(code: code, down: false))
        }
        return true
    }

    override func keyDown(with event: NSEvent) {
        guard controlling else { return super.keyDown(with: event) }
        if Self.isReleaseChord(event) {
            setControlling(false)
            onControlChange?(false)
            return
        }
        if Self.passesThrough(event) { return super.keyDown(with: event) }
        // The guest's compositor repeats held keys itself.
        guard !event.isARepeat, let code = KeyMap.evdev(macKeyCode: Int(event.keyCode)) else { return }
        send(.key(code: code, down: true))
    }

    override func keyUp(with event: NSEvent) {
        guard controlling else { return super.keyUp(with: event) }
        guard let code = KeyMap.evdev(macKeyCode: Int(event.keyCode)) else { return }
        send(.key(code: code, down: false))
    }

    override func flagsChanged(with event: NSEvent) {
        guard controlling else { return super.flagsChanged(with: event) }
        for e in modifiers.flagsChanged(macKeyCode: Int(event.keyCode)) { send(e) }
    }

    /// The guest pixel under the event, nil in the letterbox or before the
    /// first frame.
    private func guestPoint(_ event: NSEvent) -> (x: Int, y: Int)? {
        guard let front else { return nil }
        let point = convert(event.locationInWindow, from: nil)
        return GuestGeometry.guestPoint(point, viewSize: bounds.size,
                                        guestSize: CGSize(width: front.width, height: front.height), flipped: isFlipped)
    }

    private func pointer(_ event: NSEvent) {
        guard controlling, let p = guestPoint(event) else { return }
        send(.pointer(x: p.x, y: p.y))
    }

    private func button(_ event: NSEvent, down: Bool) {
        guard controlling else { return }
        if down { window?.makeFirstResponder(self) }
        // A press in the letterbox isn't the guest's; a release always is,
        // so nothing stays held.
        if down, guestPoint(event) == nil { return }
        pointer(event)
        if let button = GuestButton.evdev(event.buttonNumber) { send(.button(button, down: down)) }
    }

    override func mouseDown(with event: NSEvent) { controlling ? button(event, down: true) : super.mouseDown(with: event) }
    override func mouseUp(with event: NSEvent) { controlling ? button(event, down: false) : super.mouseUp(with: event) }
    override func rightMouseDown(with event: NSEvent) { controlling ? button(event, down: true) : super.rightMouseDown(with: event) }
    override func rightMouseUp(with event: NSEvent) { controlling ? button(event, down: false) : super.rightMouseUp(with: event) }
    override func otherMouseDown(with event: NSEvent) { controlling ? button(event, down: true) : super.otherMouseDown(with: event) }
    override func otherMouseUp(with event: NSEvent) { controlling ? button(event, down: false) : super.otherMouseUp(with: event) }
    override func mouseMoved(with event: NSEvent) { pointer(event) }
    override func mouseDragged(with event: NSEvent) { pointer(event) }
    override func rightMouseDragged(with event: NSEvent) { pointer(event) }
    override func otherMouseDragged(with event: NSEvent) { pointer(event) }

    override func scrollWheel(with event: NSEvent) {
        guard controlling else { return super.scrollWheel(with: event) }
        pointer(event)
        let clicks = scroll.add(dx: event.scrollingDeltaX, dy: event.scrollingDeltaY, precise: event.hasPreciseScrollingDeltas)
        if let e = ScrollAccumulator.event(clicks) { send(e) }
    }
}
