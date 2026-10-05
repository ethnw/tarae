// SPDX-License-Identifier: GPL-3.0-or-later
// Stacks for probes that keep them: the current task's user or kernel stack, stored once by a 64-bit hash
// of its frames (`attach.rs` reads them back by that id).
#ifndef TARAE_STACKS_BPF_H
#define TARAE_STACKS_BPF_H

#include "select.bpf.h"

// Frames kept per stack, the innermost first — a deeper stack loses its outermost frames.
#define MAX_DEPTH 64
// A stack id below this is no stack: the error `bpf_get_stack` gave (EFAULT: no user stack, a kernel thread).
#define NO_STACK 4096
#define EEXIST 17

struct st_frames {
	__u64 ips[MAX_DEPTH];
};

// Stacks the table had no room for (the numbers stay whole; those stacks come out unnamed).
struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__uint(max_entries, 1);
	__type(key, __u32);
	__type(value, __u64);
} stack_errors SEC(".maps");

// Stacks by a 64-bit hash of their frames. Not a STACK_TRACE map: that one drops a stack whose bucket another
// took (measured: one tarae run lost a 633 ms wait's stack so), a hash map compares whole keys. Preallocated
// (~17 MB): without, an update inside sched_switch can't allocate (measured: 3 stacks lost in the fixture).
struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 32768);
	__type(key, __u64);
	__type(value, struct st_frames);
} stacks SEC(".maps");

// (a stack is too big for the BPF stack: built here, one per CPU)
struct {
	__uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
	__uint(max_entries, 1);
	__type(key, __u32);
	__type(value, struct st_frames);
} scratch SEC(".maps");

// `depth` frames of `f` into `stacks`; their id = FNV-1a of them.
static __always_inline __u64 keep(struct st_frames *f, __u32 depth)
{
	__u64 h = 0xcbf29ce484222325ULL;
	for (int i = 0; i < MAX_DEPTH; i++) {
		if (i >= depth)
			break;
		h = (h ^ f->ips[i]) * 0x100000001b3ULL;
	}
	if (h < NO_STACK)
		h += NO_STACK;
	long r = bpf_map_update_elem(&stacks, &h, f, BPF_NOEXIST);
	if (r && r != -EEXIST) // (there already: the same stack)
		count_in(&stack_errors, 0);
	return h;
}

// The current task's stack (`BPF_F_USER_STACK`, or 0 for the kernel's) into `stacks`; its id, or the error.
static __always_inline __u64 stack_id(void *ctx, __u64 flags)
{
	__u32 zero = 0;
	struct st_frames *f = bpf_map_lookup_elem(&scratch, &zero);
	if (!f)
		return 1;
	long n = bpf_get_stack(ctx, f->ips, sizeof(f->ips), flags); // (zeroes the rest)
	if (n < 0)
		return -n;
	return keep(f, n / 8);
}

// The return address a function that keeps no frame of its own (a leaf: libc's getppid, its `syscall`,
// `__syscall_cancel_arch`) was entered with — still in the link register (arm64) or on top of the stack
// (x86) in the task's user registers, where a frame-pointer walk doesn't look.
static __always_inline __u64 leaf_return(void)
{
	struct pt_regs *r = (struct pt_regs *)bpf_task_pt_regs(bpf_get_current_task_btf());
	__u64 at = PT_REGS_RET_CORE(r);
#if defined(__TARGET_ARCH_x86)
	__u64 ret = 0;
	bpf_probe_read_user(&ret, sizeof(ret), (void *)at);
	return ret;
#else
	return at;
#endif
}

// The current task's user stack, its id — with the caller of a leaf put in as its second frame when the
// walk doesn't have it (measured: a getppid storm's own function was missing from all 1000 stacks).
static __always_inline __u64 user_stack_id(void *ctx)
{
	__u32 zero = 0;
	struct st_frames *f = bpf_map_lookup_elem(&scratch, &zero);
	if (!f)
		return 1;
	long n = bpf_get_stack(ctx, f->ips, sizeof(f->ips), BPF_F_USER_STACK);
	if (n < 0)
		return -n;
	__u32 depth = n / 8;
	__u64 caller = leaf_return();
	if (caller && depth >= 1 && depth < MAX_DEPTH && f->ips[1] != caller) {
		for (int i = MAX_DEPTH - 1; i > 1; i--)
			if (i <= depth)
				f->ips[i] = f->ips[i - 1];
		f->ips[1] = caller;
		depth++;
	}
	return keep(f, depth);
}

#endif
