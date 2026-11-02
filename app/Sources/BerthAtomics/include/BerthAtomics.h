#ifndef BERTH_ATOMICS_H
#define BERTH_ATOMICS_H

#include <stdint.h>

// Acquire loads (and the release stores the tests' fake writer needs) on
// shared memory another process writes: a surface ring's seqlock words.
// Swift has no atomics on raw pointers before macOS 15's Synchronization.

static inline uint64_t berth_load_acquire_u64(const void *p) {
    return __atomic_load_n((const uint64_t *)p, __ATOMIC_ACQUIRE);
}

static inline uint32_t berth_load_acquire_u32(const void *p) {
    return __atomic_load_n((const uint32_t *)p, __ATOMIC_ACQUIRE);
}

static inline void berth_store_release_u64(void *p, uint64_t v) {
    __atomic_store_n((uint64_t *)p, v, __ATOMIC_RELEASE);
}

static inline void berth_store_release_u32(void *p, uint32_t v) {
    __atomic_store_n((uint32_t *)p, v, __ATOMIC_RELEASE);
}

/// Orders the plain reads of a slot's pixels before the seqlock recheck.
static inline void berth_fence_acquire(void) {
    __atomic_thread_fence(__ATOMIC_ACQUIRE);
}

#endif
