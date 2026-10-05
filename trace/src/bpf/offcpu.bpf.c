// SPDX-License-Identifier: GPL-3.0-or-later
// Off-CPU time of the selected processes' threads (`select.bpf.h`). At a thread's switch out (it is
// `current` then): its stacks, whether it was still runnable, the time. At its switch back in: the time
// between, summed per (thread, stacks, runnable). Waits under way at the attach aren't seen; ones under way
// at the end are counted up to it by the tool (from `starts`). Written for tarae (libbpf-tools' offcputime
// is GPL-2.0-only, so nothing is copied).
#include "stacks.bpf.h"

#define TASK_RUNNING 0
#define TASK_DEAD 0x80

// The layouts below are read byte by byte in offcpu.rs — change both together.
struct oc_start {
	__u64 ts;
	__u64 ustack;
	__u64 kstack;
	__u32 tgid; // in the tool's pid namespace, as are tids below
	__u32 tid;
	__u32 preempted;
	char comm[COMM_LEN];
};

struct oc_key {
	__u64 ustack;
	__u64 kstack;
	__u32 tgid;
	__u32 tid;
	__u32 preempted;
	char comm[COMM_LEN];
	__u32 pad;
};

struct oc_val {
	__u64 ns;
	__u64 count;
	__u64 max_ns;
};

// What couldn't be counted (numbers short): `starts` full, `offcpu` full.
enum { E_STARTS, E_OFFCPU, E_N };

struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__uint(max_entries, E_N);
	__type(key, __u32);
	__type(value, __u64);
} errors SEC(".maps");

// Keyed by the kernel's (global) tid: the switch in only has `next`, not its namespaced ids.
struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 65536);
	__type(key, __u32);
	__type(value, struct oc_start);
} starts SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 16384);
	__type(key, struct oc_key);
	__type(value, struct oc_val);
} offcpu SEC(".maps");

static __always_inline void switched_out(void *ctx, struct task_struct *prev, struct bpf_pidns_info *ns,
					 __u32 state, __u64 now)
{
	struct oc_start s = {};
	s.ts = now;
	s.tgid = ns->tgid;
	s.tid = ns->pid;
	s.ustack = user_stack_id(ctx);
	s.kstack = stack_id(ctx, 0);
	s.preempted = state == TASK_RUNNING;
	bpf_get_current_comm(&s.comm, sizeof(s.comm));
	__u32 tid = BPF_CORE_READ(prev, pid);
	if (bpf_map_update_elem(&starts, &tid, &s, BPF_ANY))
		count_in(&errors, E_STARTS);
	mark_seen(s.tgid);
}

SEC("tp_btf/sched_switch")
int BPF_PROG(on_switch, bool preempt, struct task_struct *prev, struct task_struct *next)
{
	const struct sel_config *c = sel_settings();
	if (!c)
		return 0;
	__u64 now = bpf_ktime_get_ns();
	__u32 state = BPF_CORE_READ(prev, __state);
	struct bpf_pidns_info ns = {};
	if (!(state & TASK_DEAD) && selected(prev, BPF_CORE_READ(prev, tgid), c, &ns))
		switched_out(ctx, prev, &ns, state, now);

	__u32 tid = BPF_CORE_READ(next, pid);
	struct oc_start *s = bpf_map_lookup_elem(&starts, &tid);
	if (!s)
		return 0;
	struct oc_key k = {};
	k.ustack = s->ustack;
	k.kstack = s->kstack;
	k.tgid = s->tgid;
	k.tid = s->tid;
	k.preempted = s->preempted;
	__builtin_memcpy(k.comm, s->comm, COMM_LEN);
	__u64 delta = now - s->ts;
	bpf_map_delete_elem(&starts, &tid);

	struct oc_val *v = bpf_map_lookup_elem(&offcpu, &k);
	if (!v) {
		struct oc_val fresh = {};
		bpf_map_update_elem(&offcpu, &k, &fresh, BPF_NOEXIST);
		v = bpf_map_lookup_elem(&offcpu, &k);
		if (!v) {
			count_in(&errors, E_OFFCPU);
			return 0;
		}
	}
	__sync_fetch_and_add(&v->ns, delta);
	__sync_fetch_and_add(&v->count, 1);
	if (delta > v->max_ns)
		v->max_ns = delta;
	return 0;
}
