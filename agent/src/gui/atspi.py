# berth: the accessibility tree of a GUI session, through AT-SPI (the
# GObject-introspection bindings, python3-gi + the Atspi typelib; no pyatspi).
# Run by the agent (agent/src/gui/mod.rs) inside the session, as the
# machine's user, with the session's D-Bus. Argument: JSON options.
#   {"mode":"enable"}  ask toolkits that wait for a screen reader (Chromium,
#                      Electron, Firefox) to expose their tree
#   {"mode":"tree","depth":12,"max":4000,"app":null,"pids":[..],
#    "windows":[{"pid":..,"title":..,"frame":[x,y,w,h]}]}
# Prints a Node (docs/gui-protocol.md) as JSON.
#
# On Wayland a client doesn't know where its windows are, so toolkits report
# element positions relative to their window (CoordType.WINDOW); the agent
# passes where the compositor put each window, and frames are moved by that.
import json
import sys

try:
    import gi
    gi.require_version("Atspi", "2.0")
    from gi.repository import Atspi
except Exception as err:
    print("AT-SPI isn't available (python3-gi and the Atspi typelib): %s" % err, file=sys.stderr)
    sys.exit(2)

opts = json.loads(sys.argv[1])

if opts.get("mode") == "enable":
    from gi.repository import Gio, GLib
    try:
        bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
        for prop in ("IsEnabled", "ScreenReaderEnabled"):
            bus.call_sync("org.a11y.Bus", "/org/a11y/bus", "org.freedesktop.DBus.Properties", "Set",
                          GLib.Variant("(ssv)", ("org.a11y.Status", prop, GLib.Variant("b", True))),
                          None, Gio.DBusCallFlags.NONE, 3000, None)
    except Exception as err:
        print("enabling accessibility: %s" % err, file=sys.stderr)
    sys.exit(0)

S = Atspi.StateType
TOPLEVEL = {"frame", "window", "dialog", "alert", "file chooser"}
max_depth = int(opts.get("depth") or 12)
max_nodes = int(opts.get("max") or 4000)
want_app = (opts.get("app") or "").lower()
want_pids = set(opts.get("pids") or [])
windows = opts.get("windows") or []
used = set()
count = 0


def window_offset(pid, title):
    """Where the compositor put this toplevel: same pid and title, else the
    pid's first window not matched yet."""
    best = None
    for i, w in enumerate(windows):
        if w.get("pid") != pid:
            continue
        if w.get("title") == title and i not in used:
            best = i
            break
        if best is None and i not in used:
            best = i
    if best is None:
        return (0, 0)
    used.add(best)
    f = windows[best]["frame"]
    return (f[0], f[1])


def ifaces(acc):
    try:
        return set(acc.get_interfaces())
    except Exception:
        return set()


def frame(acc, kinds, off):
    if "Component" not in kinds:
        return [0, 0, 0, 0]
    try:
        r = Atspi.Component.get_extents(acc, Atspi.CoordType.WINDOW)
        # Unmapped widgets (a stack's other pages) can report nonsense.
        if not (0 <= r.width <= 100000 and 0 <= r.height <= 100000):
            return [0, 0, 0, 0]
        return [r.x + off[0], r.y + off[1], r.width, r.height]
    except Exception:
        return [0, 0, 0, 0]


def value(acc, kinds, role):
    try:
        if "Text" in kinds and role not in ("label", "push button", "heading", "static"):
            n = Atspi.Text.get_character_count(acc)
            return Atspi.Text.get_text(acc, 0, min(n, 2000))
        if "Value" in kinds:
            return str(Atspi.Value.get_current_value(acc))
    except Exception:
        pass
    return None


def node(acc, depth, off):
    global count
    count += 1
    try:
        role = acc.get_role_name() or ""
    except Exception:
        role = ""
    try:
        label = acc.get_name() or acc.get_description() or ""
    except Exception:
        label = ""
    try:
        states = acc.get_state_set()
        enabled = states.contains(S.ENABLED) or states.contains(S.SENSITIVE)
        focused = states.contains(S.FOCUSED)
    except Exception:
        states, enabled, focused = None, False, False
    kinds = ifaces(acc)
    if role in TOPLEVEL and depth <= 2:
        try:
            off = window_offset(acc.get_process_id(), label)
        except Exception:
            pass
    out = {"role": role, "label": label, "value": value(acc, kinds, role), "frame": frame(acc, kinds, off),
           "enabled": bool(enabled), "focused": bool(focused), "children": []}
    if depth >= max_depth:
        return out
    try:
        n = acc.get_child_count()
    except Exception:
        n = 0
    for i in range(min(n, 500)):
        if count >= max_nodes:
            break
        try:
            child = acc.get_child_at_index(i)
            # Hidden pages of stacks and closed menus: not on screen.
            if child is None or not child.get_state_set().contains(S.SHOWING):
                continue
        except Exception:
            continue
        out["children"].append(node(child, depth + 1, off))
    return out


desktop = Atspi.get_desktop(0)
root = {"role": "desktop frame", "label": "", "value": None, "frame": opts.get("screen") or [0, 0, 0, 0],
        "enabled": True, "focused": False, "children": []}
for i in range(desktop.get_child_count()):
    try:
        app = desktop.get_child_at_index(i)
        name = app.get_name() or ""
        pid = app.get_process_id()
    except Exception:
        continue
    if want_app and want_app not in name.lower() and pid not in want_pids:
        continue
    root["children"].append(node(app, 1, (0, 0)))
print(json.dumps(root))
