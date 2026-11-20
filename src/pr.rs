//! Parsing a GitHub pull-request reference out of spawn input.
//!
//! Pure and host-agnostic: the path shape (`…/<owner>/<repo>/pull/<number>`) is what
//! identifies a PR, not the hostname, so GitHub Enterprise URLs work the same as
//! github.com ones. Which remote that `owner/repo` actually maps to is a git question,
//! answered in `worktree::remote_for_repo` — nothing here shells out.

/// A pull request the user asked to spawn an agent for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrRef {
    /// The `owner/repo` the URL named, or `None` for the bare `#N` form — which means
    /// "a PR of the repo I'm already in" and so resolves against `origin`.
    pub repo: Option<(String, String)>,
    pub number: u32,
}

impl PrRef {
    /// The local branch a fetched PR lands on. Deliberately derived from the number
    /// rather than the PR's head branch name: `refs/pull/N/head` doesn't carry that
    /// name, and asking for it would mean a GitHub API call (and a `gh` dependency)
    /// just to pick a label.
    pub fn branch(&self) -> String {
        format!("pr-{}", self.number)
    }

    /// The refspec that puts the PR's head on that local branch. Fork PRs included:
    /// `refs/pull/N/head` lives on the *base* repo whatever the head repo is.
    ///
    /// Not forced (`+`): a PR that was force-pushed since the last fetch makes git
    /// reject this as a non-fast-forward rather than silently rewriting a branch the
    /// user may have built on.
    pub fn refspec(&self) -> String {
        format!("refs/pull/{n}/head:refs/heads/pr-{n}", n = self.number)
    }
}

/// Parse a PR reference off the FRONT of `input`, returning it with the unparsed
/// remainder (which the caller reads as the agent's initial prompt).
///
/// Accepted: `https://github.com/owner/repo/pull/12`, the same without a scheme, the
/// `owner/repo/pull/12` shorthand, and `#12` for the current repo. A trailing URL tail
/// (`/files`, `#issuecomment-…`, `?w=1`) is consumed as part of the reference, so
/// pasting the URL straight out of a browser works.
pub fn parse_pr_ref(input: &str) -> Option<(PrRef, &str)> {
    let s = input.trim_start();
    if let Some(digits) = s.strip_prefix('#') {
        let (number, rest) = take_number(digits)?;
        return Some((PrRef { repo: None, number }, rest));
    }

    let path = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(s);
    let marker = path.find("/pull/")?;
    let (left, right) = path.split_at(marker);
    let (number, rest) = take_number(&right["/pull/".len()..])?;

    // `left` is `[host/]owner/repo`: take the last two segments, so a URL with a
    // hostname and the bare `owner/repo/pull/N` shorthand both work.
    let segs: Vec<&str> = left.split('/').filter(|s| !s.is_empty()).collect();
    let [.., owner, repo] = segs.as_slice() else {
        return None;
    };

    Some((
        PrRef {
            repo: Some((owner.to_string(), repo.to_string())),
            number,
        },
        // Anything glued to the number that still looks like part of the URL belongs to
        // the reference, not to the prompt.
        match rest.starts_with(['/', '#', '?']) {
            true => rest.split_once(char::is_whitespace).map_or("", |(_, r)| r),
            false => rest,
        },
    ))
}

/// Leading ASCII digits as a number, plus what follows. `None` when there are none, or
/// when they don't fit a PR number.
fn take_number(s: &str) -> Option<(u32, &str)> {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    s[..end].parse().ok().map(|n| (n, &s[end..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(owner: &str, repo: &str, number: u32) -> PrRef {
        PrRef {
            repo: Some((owner.to_string(), repo.to_string())),
            number,
        }
    }

    #[test]
    fn parses_a_browser_url() {
        assert_eq!(
            parse_pr_ref("https://github.com/guimochila/hydra/pull/36"),
            Some((pr("guimochila", "hydra", 36), ""))
        );
    }

    #[test]
    fn parses_urls_a_browser_actually_hands_you() {
        // A tab left on the Files/comment view, or a scheme-less copy.
        for input in [
            "https://github.com/guimochila/hydra/pull/36/files",
            "https://github.com/guimochila/hydra/pull/36#issuecomment-123",
            "https://github.com/guimochila/hydra/pull/36?w=1",
            "http://github.com/guimochila/hydra/pull/36",
            "github.com/guimochila/hydra/pull/36",
            "guimochila/hydra/pull/36",
        ] {
            assert_eq!(
                parse_pr_ref(input).map(|(p, _)| p),
                Some(pr("guimochila", "hydra", 36)),
                "{input}"
            );
        }
    }

    #[test]
    fn host_is_not_hardcoded_so_enterprise_urls_work() {
        assert_eq!(
            parse_pr_ref("https://github.tomtom.com/maps/routing/pull/9").map(|(p, _)| p),
            Some(pr("maps", "routing", 9))
        );
    }

    #[test]
    fn the_hash_form_means_the_current_repo() {
        assert_eq!(
            parse_pr_ref("#36"),
            Some((
                PrRef {
                    repo: None,
                    number: 36
                },
                ""
            ))
        );
    }

    #[test]
    fn returns_the_remainder_for_the_caller_to_read_as_a_prompt() {
        let cases = [
            ("https://github.com/o/r/pull/7: review it", ": review it"),
            ("https://github.com/o/r/pull/7 review it", " review it"),
            // No space before the colon: the tail isn't URL-shaped, so it stays.
            ("https://github.com/o/r/pull/7:review it", ":review it"),
            // A URL tail IS consumed up to the whitespace, separator and all.
            (
                "https://github.com/o/r/pull/7/files: review it",
                "review it",
            ),
            ("#7: review it", ": review it"),
        ];
        for (input, want) in cases {
            assert_eq!(
                parse_pr_ref(input).map(|(_, rest)| rest),
                Some(want),
                "{input}"
            );
        }
    }

    #[test]
    fn branch_and_refspec_are_derived_from_the_number() {
        let p = pr("o", "r", 36);
        assert_eq!(p.branch(), "pr-36");
        // Unforced on purpose: a force-pushed PR must fail loudly, not rewrite history
        // under a branch the user may have committed to.
        assert_eq!(p.refspec(), "refs/pull/36/head:refs/heads/pr-36");
        assert!(!p.refspec().starts_with('+'));
    }

    #[test]
    fn plain_branch_names_are_not_pr_refs() {
        // Ordinary spawn input must keep falling through to the name path — including a
        // branch with `pull` in it, which lacks the owner/repo segments in front.
        for input in [
            "feat/pagination",
            "fix-pull-flakes",
            "pull/36",
            "some/pull/36x",
            "#",
            "#abc",
            "https://github.com/guimochila/hydra/issues/36",
        ] {
            assert_eq!(parse_pr_ref(input), None, "{input}");
        }
    }

    #[test]
    fn a_number_too_big_to_be_a_pr_is_not_one() {
        assert_eq!(parse_pr_ref("#99999999999999999999"), None);
    }
}
