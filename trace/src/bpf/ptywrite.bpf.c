// SPDX-License-Identifier: GPL-3.0-or-later
// Writes of the selected processes to a terminal (`select.bpf.h`): every `tty_write` — what write(2) and
// writev(2) on a tty or pty end in — from entry to return, so a write that waits for room (a terminal that
// reads slowly: the pty full) counts its wait. Per (thread, user stack): calls, bytes, time, the longest,
// and a log2 histogram in microseconds.
//
// fentry/fexit on `tty_write(struct kiocb *, struct iov_iter *)` — its shape since Linux 5.11 (before: a
// `char __user *` and a length). A kernel where it's inlined or renamed fails at attach, saying so.
#include "stacks.bpf.h"

#define BUCKETS 32

// The layouts below are read byte by byte in ptywrite.rs — change both together.
struct pw_start {
	__u64 ts;
	__u64 ustack;
	__u32 tgid; // in the tool's pid namespace, as are tids below
	__u32 tid;
	char comm[COMM_LEN];
};

struct pw_key {
	__u64 ustack;
	__u32 tgid;
	__u32 tid;
	char comm[COMM_LEN];
};

struct pw_val {
	__u64 count;
	__u64 ns;
	__u64 max_ns;
	__u64 bytes;
	__u64 hist[BUCKETS]; // [b]: calls of 2^b ≤ µs < 2^(b+1) (b = 0 also under 1 µs)
};

// What couldn't be counted (numbers short): `starts` full, `writes` full.
enum { E_STARTS, E_WRITES, E_N };

struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__uint(max_entries, E_N);
	__type(key, __u32);
	__type(value, __u64);
} errors SEC(".maps");

// Keyed by the kernel's (global) tid.
struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 16384);
	__type(key, __u32);
	__type(value, struct pw_start);
} starts SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 8192);
	__type(key, struct pw_key);
	__type(value, struct pw_val);
} writes SEC(".maps");

SEC("fentry/tty_write")
int BPF_PROG(on_tty_write, struct kiocb *iocb, struct iov_iter *from)
{
	const struct sel_config *c = sel_settings();
	if (!c)
		return 0;
	__u64 pid_tgid = bpf_get_current_pid_tgid();
	struct bpf_pidns_info ns = {};
	if (!selected(bpf_get_current_task_btf(), pid_tgid >> 32, c, &ns))
		return 0;
	struct pw_start s = {};
	s.ts = bpf_ktime_get_ns();
	s.ustack = user_stack_id(ctx);
	s.tgid = ns.tgid;
	s.tid = ns.pid;
	bpf_get_current_comm(&s.comm, sizeof(s.comm));
	__u32 tid = pid_tgid;
	if (bpf_map_update_elem(&starts, &tid, &s, BPF_ANY))
		count_in(&errors, E_STARTS);
	mark_seen(s.tgid);
	return 0;
}

SEC("fexit/tty_write")
int BPF_PROG(on_tty_write_done, struct kiocb *iocb, struct iov_iter *from, ssize_t ret)
{
	__u32 tid = bpf_get_current_pid_tgid();
	struct pw_start *s = bpf_map_lookup_elem(&starts, &tid);
	if (!s)
		return 0;
	__u64 delta = bpf_ktime_get_ns() - s->ts;
	struct pw_key k = {};
	k.ustack = s->ustack;
	k.tgid = s->tgid;
	k.tid = s->tid;
	__builtin_memcpy(k.comm, s->comm, COMM_LEN);
	bpf_map_delete_elem(&starts, &tid);

	struct pw_val *v = bpf_map_lookup_elem(&writes, &k);
	if (!v) {
		struct pw_val fresh = {};
		bpf_map_update_elem(&writes, &k, &fresh, BPF_NOEXIST);
		v = bpf_map_lookup_elem(&writes, &k);
		if (!v) {
			count_in(&errors, E_WRITES);
			return 0;
		}
	}
	__u64 us = delta / 1000;
	__u32 b = 0;
	for (int i = 0; i < BUCKETS - 1; i++) {
		if (us < 2)
			break;
		us >>= 1;
		b++;
	}
	__sync_fetch_and_add(&v->count, 1);
	__sync_fetch_and_add(&v->ns, delta);
	if (ret > 0)
		__sync_fetch_and_add(&v->bytes, ret);
	if (delta > v->max_ns)
		v->max_ns = delta;
	__sync_fetch_and_add(&v->hist[b & (BUCKETS - 1)], 1);
	return 0;
}
