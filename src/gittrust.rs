//! The card for a repository's own git commands (`gitrepo`: its config's command-running keys and the hooks
//! in its git folder, held on every run of tarae's git until answered): `y` trusts them, `n` holds them back
//! and isn't asked again for these values. Answers are kept as `.tarae.toml`'s are (`trust.rs`), by the
//! repository's config file and a digest of what was found — a key or value changed since asks again.
//! Asked whenever a resolve finds them unanswered, whichever git call it was for (`gitrepo::set_asker`) —
//! and again for a repository kept as resolved when a session's checkout is first watched (`git::watch`),
//! a file's repository read for its gutter (`git_load_base`) or the git tab's status read (a card gone
//! unanswered comes back): resolved on a worker, the card on the main loop.

use std::path::{Path, PathBuf};

use crate::editor::Editor;
use crate::gitrepo::Repo;
use crate::offer::{Offer, What};
use crate::trust::{Digest, SHOWN_MAX};

pub const TITLE: &str = "Let this repository's git settings run commands?";

/// What the card shows and answers about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ask {
    /// The repository's config file — what answers are kept by.
    pub config: PathBuf,
    /// The checkout it's about (its git folder when there's none).
    pub place: PathBuf,
    pub digest: Digest,
    /// (key, value, when it runs).
    pub items: Vec<(String, String, &'static str)>,
    /// The files their values run, the answer covering each as it is now (`Local::covers`) — and whether
    /// it's there. The git folder's hooks are items of their own.
    pub files: Vec<(PathBuf, bool)>,
    /// A filter among them: files show as git stores them until `y` (LFS pointers, say).
    pub filters: bool,
}

impl Ask {
    pub fn of(repo: &Repo) -> Ask {
        let items: Vec<(String, String, &'static str)> =
            repo.local.found.iter().map(|(k, v)| (k.clone(), v.clone(), crate::gitrepo::runs(k))).collect();
        let filters = items.iter().any(|(k, ..)| k.starts_with("filter."));
        let place = repo.work_tree.clone().unwrap_or_else(|| repo.git_dir.clone());
        let hooks = repo.common_dir.join("hooks");
        let files = repo.local.covers.iter().filter(|(p, _)| !p.starts_with(&hooks));
        let files = files.map(|(p, sum)| (p.clone(), sum.is_some())).collect();
        Ask { config: repo.config_file(), place, digest: repo.local.digest(), items, files, filters }
    }

    /// Every value short enough to show whole: else only `n`.
    pub fn showable(&self) -> bool {
        self.items.iter().all(|(_, v, _)| v.chars().count() <= SHOWN_MAX)
    }

    /// The card (`root` = the editor's: where the place is said from). Each key and value one line, what
    /// would draw as something else spelled out (`trust::visible_line`): text from the repository can't
    /// make a line of the card's own, nor shift the decided lines off the items.
    pub fn card(&self, root: &Path) -> Offer {
        let n = self.items.len();
        let line = crate::trust::visible_line;
        let place = line(&crate::chat::shown(root, &self.place));
        let mut q = format!("{place} — its own git settings run these as you:\n");
        for (k, v, _) in &self.items {
            match v.chars().count() {
                c if c > SHOWN_MAX => {
                    q += &format!("\n  {} = ({c} characters — too long to show, so only n)", line(k))
                }
                _ => q += &format!("\n  {} = {}", line(k), line(v)),
            }
        }
        if !self.files.is_empty() {
            let names: Vec<String> = self
                .files
                .iter()
                .map(|(p, there)| {
                    let name = line(&crate::chat::shown(&self.place, p));
                    if *there { name } else { format!("{name} (not there yet)") }
                })
                .collect();
            q += &format!("\n\nAs these files are now: {} — a change to one asks again.", names.join(", "));
        }
        let mut when: Vec<&str> = Vec::new();
        for (.., r) in &self.items {
            if !when.contains(r) {
                when.push(r);
            }
        }
        q += &format!("\n\nThey run {}.", when.join("; "));
        q += "\nHeld back until you say yes: tarae's git runs without them";
        if self.filters {
            q += " — filtered files show as git stores them (LFS pointers, say)";
        }
        q += ". n doesn't ask again for these values.";
        Offer::new(TITLE.into(), &q, "", "Repository commands".into(), What::GitTrust(Box::new(self.clone())))
            .deciding(2..2 + n)
    }
}

impl Editor {
    /// A repository resolved on a worker: its card when what it runs hasn't been answered — one up per
    /// repository, in front or parked in any session. Whether it went up.
    pub(crate) fn git_trust_check(&mut self, repo: &Repo) -> bool {
        let config = repo.config_file();
        if !repo.asks || crate::gitrepo::answered_since(&config, repo.local.digest()) {
            return false;
        }
        let parked = self.sessions.iter().flat_map(|s| &s.parked);
        let up = self
            .offers
            .iter()
            .chain(&self.session.parked)
            .chain(parked)
            .any(|o| matches!(&o.what, What::GitTrust(a) if a.config == config));
        if !up {
            let o = Ask::of(repo).card(&self.session.root);
            self.push_offer(o);
        }
        !up
    }

    /// A resolve told it found `repo`'s commands unanswered (`gitrepo::set_asker`): its card — up already,
    /// nothing changed on screen.
    pub(crate) fn git_trust_told(&mut self, repo: &Repo) {
        if !self.git_trust_check(repo) {
            self.unchanged = true;
        }
    }

    /// The card answered: kept (since tarae started at once, in the store on a worker), the repository
    /// resolved again on its next git — and the gutter read again, with them or without.
    pub(crate) fn git_trust_answer(&mut self, id: u64, ask: Ask, trusted: bool) {
        self.offer_finished(id, Ok(()));
        crate::gitrepo::answered(&ask.config, ask.digest, trusted);
        if let Some(path) = self.trust_path.clone() {
            let (config, digest) = (ask.config.clone(), ask.digest);
            self.events.jobs().spawn(move || {
                let r = crate::trust::record(path, config, digest, trusted);
                move |ed: &mut Editor| {
                    if let Err(e) = r {
                        ed.set_error(format!("couldn't keep the answer: {e}"));
                    }
                }
            });
        }
        self.git_reload_bases();
        // (a submodule's answer: the git tab shows its changes now, or says they stay hidden)
        if self.session.git_status.is_some() {
            self.git_refresh();
        }
        match trusted {
            true => self.set_success("Trusted — this repository's own git settings run"),
            false => self.set_status("Held back — tarae's git runs without them"),
        }
    }
}

#[cfg(test)]
mod tests;
