// SPDX-License-Identifier: GPL-3.0-or-later
// The selected processes' syscalls (`select.bpf.h`): per (thread, syscall number) the calls, the time from
// entry to return, the longest, and the failures. Raw syscall tracepoints with BTF — no tracefs needed. A
// call under way at the attach isn't counted; one that never returns (exit, a successful execve's old image)
// leaves a start the next call of that thread replaces. For the syscalls `stack_for` names (`--stacks`), the
// caller's user stack too — summed per stack, so a storm of one syscall says where it comes from.
#include "stacks.bpf.h"

#define SYSCALLS 1024

// The layouts below are read byte by byte in syscalls.rs — change both together.
struct sc_start {
	__u64 ts;
	__u64 ustack; // 0: not asked for
	__u32 nr;
	__u32 tgid; // in the tool's pid namespace, as are tids below
	__u32 tid;
	char comm[COMM_LEN];
};

struct sc_key {
	__u64 ustack;
	__u32 tgid;
	__u32 tid;
	__u32 nr;
	char comm[COMM_LEN];
	__u32 pad;
};

struct sc_val {
	__u64 count;
	__u64 ns;
	__u64 max_ns;
	__u64 failed;
};

// What couldn't be counted (numbers short): `starts` full, `calls` full.
enum { E_STARTS, E_CALLS, E_N };

struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__uint(max_entries, E_N);
	__type(key, __u32);
	__type(value, __u64);
} errors SEC(".maps");

// Keyed by the kernel's (global) tid.
struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 65536);
	__type(key, __u32);
	__type(value, struct sc_start);
} starts SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 16384);
	__type(key, struct sc_key);
	__type(value, struct sc_val);
} calls SEC(".maps");

// By syscall number: 1 = keep its callers' user stacks (filled by the tool).
struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__uint(max_entries, SYSCALLS);
	__type(key, __u32);
	__type(value, __u8);
} stack_for SEC(".maps");

SEC("tp_btf/sys_enter")
int BPF_PROG(on_enter, struct pt_regs *regs, long id)
{
	const struct sel_config *c = sel_settings();
	if (!c)
		return 0;
	__u64 pid_tgid = bpf_get_current_pid_tgid();
	struct bpf_pidns_info ns = {};
	if (!selected(bpf_get_current_task_btf(), pid_tgid >> 32, c, &ns))
		return 0;
	struct sc_start s = {};
	s.nr = id;
	__u8 *want = bpf_map_lookup_elem(&stack_for, &s.nr);
	if (want && *want) {
		s.ustack = user_stack_id(ctx);
		mark_seen(ns.tgid);
	}
	s.ts = bpf_ktime_get_ns();
	s.tgid = ns.tgid;
	s.tid = ns.pid;
	bpf_get_current_comm(&s.comm, sizeof(s.comm));
	__u32 tid = pid_tgid;
	if (bpf_map_update_elem(&starts, &tid, &s, BPF_ANY))
		count_in(&errors, E_STARTS);
	return 0;
}

SEC("tp_btf/sys_exit")
int BPF_PROG(on_exit, struct pt_regs *regs, long ret)
{
	__u32 tid = bpf_get_current_pid_tgid();
	struct sc_start *s = bpf_map_lookup_elem(&starts, &tid);
	if (!s)
		return 0;
	__u64 delta = bpf_ktime_get_ns() - s->ts;
	struct sc_key k = {};
	k.ustack = s->ustack;
	k.tgid = s->tgid;
	k.tid = s->tid;
	k.nr = s->nr;
	__builtin_memcpy(k.comm, s->comm, COMM_LEN);
	bpf_map_delete_elem(&starts, &tid);

	struct sc_val *v = bpf_map_lookup_elem(&calls, &k);
	if (!v) {
		struct sc_val fresh = {};
		bpf_map_update_elem(&calls, &k, &fresh, BPF_NOEXIST);
		v = bpf_map_lookup_elem(&calls, &k);
		if (!v) {
			count_in(&errors, E_CALLS);
			return 0;
		}
	}
	__sync_fetch_and_add(&v->count, 1);
	__sync_fetch_and_add(&v->ns, delta);
	if (delta > v->max_ns)
		v->max_ns = delta;
	if (ret < 0 && ret > -4096)
		__sync_fetch_and_add(&v->failed, 1);
	return 0;
}
