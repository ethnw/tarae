// SPDX-License-Identifier: GPL-3.0-or-later
// Which processes a probe traces — shared by every probe (`trace/src/attach.rs` is its other half).
//
// Those in `tracked` (by the kernel's global tgid), which fills itself from
//  - `seeds`: pids in the tool's own pid namespace (`--pid`, and with `--children` its children at attach),
//    tracked at their first event — the tool may be in a container, where global pids aren't known;
//  - `settings.comm`: processes whose group leader has that comm (tarae names its worker threads);
//  - with `settings.children`, every fork of a tracked process (`on_fork`).
// An exiting process leaves `tracked` and `seeds`, so a reused pid isn't followed. A probe calls `selected`
// on the current task and `mark_seen` for a process whose stacks it keeps (the tool reads its maps then).
#ifndef TARAE_SELECT_BPF_H
#define TARAE_SELECT_BPF_H

#include "vmlinux.h"
#include <bpf/bpf_core_read.h>
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>

#define COMM_LEN 16

char LICENSE[] SEC("license") = "GPL";

// The layout is written byte by byte in attach.rs — change both together.
struct sel_config {
	char comm[COMM_LEN]; // empty: no choosing by comm
	__u64 pidns_dev;
	__u64 pidns_ino;
	__u32 children;
	__u32 pad;
};

// What the selection couldn't do: `tracked` full (numbers short), switches of the chosen comm in other pid
// namespaces (another container's — left out, said).
enum { S_TRACKED, S_OTHER_NS, S_N };

struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__uint(max_entries, 1);
	__type(key, __u32);
	__type(value, struct sel_config);
} settings SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__uint(max_entries, S_N);
	__type(key, __u32);
	__type(value, __u64);
} sel_errors SEC(".maps");

// Pids in the tool's namespace, filled by the tool.
struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 4096);
	__type(key, __u32);
	__type(value, __u8);
} seeds SEC(".maps");

// Global tgids being traced.
struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 16384);
	__type(key, __u32);
	__type(value, __u8);
} tracked SEC(".maps");

// Processes seen (pids in the tool's namespace): the tool polls it and reads each one's maps while it lives.
struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, 4096);
	__type(key, __u32);
	__type(value, __u8);
} seen SEC(".maps");

static __always_inline void count_in(void *errors, __u32 which)
{
	__u64 *n = bpf_map_lookup_elem(errors, &which);
	if (n)
		__sync_fetch_and_add(n, 1);
}

static __always_inline const struct sel_config *sel_settings(void)
{
	__u32 zero = 0;
	return bpf_map_lookup_elem(&settings, &zero);
}

static __always_inline bool comm_matches(struct task_struct *t, const struct sel_config *c)
{
	char comm[COMM_LEN];
	BPF_CORE_READ_STR_INTO(&comm, t, group_leader, comm);
	for (int i = 0; i < COMM_LEN; i++) {
		if (comm[i] != c->comm[i])
			return false;
		if (comm[i] == 0)
			break;
	}
	return true;
}

// Whether the current task `t` (of global tgid `tgid`) is traced; `ns` gets its namespaced ids.
static __always_inline bool selected(struct task_struct *t, __u32 tgid, const struct sel_config *c,
				     struct bpf_pidns_info *ns)
{
	bool by_comm = c->comm[0] && comm_matches(t, c);
	if (bpf_get_ns_current_pid_tgid(c->pidns_dev, c->pidns_ino, ns, sizeof(*ns))) {
		if (by_comm || bpf_map_lookup_elem(&tracked, &tgid))
			count_in(&sel_errors, S_OTHER_NS);
		return false;
	}
	if (bpf_map_lookup_elem(&tracked, &tgid))
		return true;
	if (!by_comm && !bpf_map_lookup_elem(&seeds, &ns->tgid))
		return false;
	// (by comm without children: asked again at each event, so a fork's child stops matching at its exec)
	if (!by_comm || c->children) {
		__u8 one = 1;
		if (bpf_map_update_elem(&tracked, &tgid, &one, BPF_ANY))
			count_in(&sel_errors, S_TRACKED);
	}
	return true;
}

static __always_inline void mark_seen(__u32 ns_tgid)
{
	if (!bpf_map_lookup_elem(&seen, &ns_tgid)) {
		__u8 one = 1;
		bpf_map_update_elem(&seen, &ns_tgid, &one, BPF_NOEXIST);
	}
}

// A tracked process's new process is tracked too, with `children` (a new thread is the same tgid already).
SEC("tp_btf/sched_process_fork")
int BPF_PROG(on_fork, struct task_struct *parent, struct task_struct *child)
{
	const struct sel_config *c = sel_settings();
	if (!c || !c->children)
		return 0;
	__u32 p = BPF_CORE_READ(parent, tgid), ch = BPF_CORE_READ(child, tgid);
	if (p == ch)
		return 0;
	if (!bpf_map_lookup_elem(&tracked, &p)) {
		// (a seed that hasn't had an event since the attach isn't in `tracked` yet; the parent is `current`)
		struct bpf_pidns_info ns = {};
		if (bpf_get_ns_current_pid_tgid(c->pidns_dev, c->pidns_ino, &ns, sizeof(ns)) ||
		    !bpf_map_lookup_elem(&seeds, &ns.tgid))
			return 0;
	}
	__u8 one = 1;
	if (bpf_map_update_elem(&tracked, &ch, &one, BPF_ANY))
		count_in(&sel_errors, S_TRACKED);
	return 0;
}

// A process that ends (its leader exiting — `current`) is forgotten: its pid may come back as another one.
SEC("tp_btf/sched_process_exit")
int BPF_PROG(on_process_exit, struct task_struct *p)
{
	__u32 tgid = BPF_CORE_READ(p, tgid);
	if (BPF_CORE_READ(p, pid) != tgid)
		return 0;
	bpf_map_delete_elem(&tracked, &tgid);
	const struct sel_config *c = sel_settings();
	struct bpf_pidns_info ns = {};
	if (c && !bpf_get_ns_current_pid_tgid(c->pidns_dev, c->pidns_ino, &ns, sizeof(ns)))
		bpf_map_delete_elem(&seeds, &ns.tgid);
	return 0;
}

#endif
