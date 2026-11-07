//! rsync-style include/exclude rules.
//!
//! Patterns follow rsync: a leading "/" anchors the pattern at the top of the
//! transfer, a trailing "/" matches only directories, "*" and "?" stop at
//! slashes while "**" does not, and "dir/***" matches a directory together
//! with everything inside it. A pattern without a slash is matched against
//! the last path component at any depth; an unanchored pattern with slashes
//! is matched against the trailing components of the path. Rules are checked
//! in order, the first match decides, and an unmatched path is included.

#[derive(Clone, Debug)]
struct Rule {
    include: bool,
    pat: Vec<u8>,
    anchored: bool,
    dir_only: bool,
    /// Contains "**", so it may match starting at any depth.
    wild2: bool,
    /// How many path components the pattern spans.
    components: usize,
}

#[derive(Clone, Debug, Default)]
pub struct Filter {
    rules: Vec<Rule>,
    /// For a source given without a trailing slash, rsync's transfer root is
    /// its parent, so patterns see paths starting with the directory's name.
    prefix: Vec<u8>,
}

impl Filter {
    pub fn add(&mut self, include: bool, pattern: &str) -> Result<(), String> {
        let mut p = pattern.as_bytes();
        let anchored = p.first() == Some(&b'/');
        if anchored {
            p = &p[1..];
        }
        if let Some(base) = p.strip_suffix(b"/***") {
            self.push(include, base, anchored, true);
            let mut all = base.to_vec();
            all.extend_from_slice(b"/**");
            self.push(include, &all, anchored, false);
            return Ok(());
        }
        let dir_only = p.ends_with(b"/");
        while p.ends_with(b"/") {
            p = &p[..p.len() - 1];
        }
        if p.is_empty() {
            return Err(format!("invalid pattern {pattern:?}"));
        }
        self.push(include, p, anchored, dir_only);
        Ok(())
    }

    fn push(&mut self, include: bool, pat: &[u8], anchored: bool, dir_only: bool) {
        self.rules.push(Rule {
            include,
            pat: pat.to_vec(),
            anchored,
            dir_only,
            wild2: pat.windows(2).any(|w| w == b"**"),
            components: pat.iter().filter(|&&c| c == b'/').count() + 1,
        });
    }

    /// Parses a filter-file line: "+ PATTERN", "- PATTERN", "include PATTERN",
    /// "exclude PATTERN", or a bare pattern that gets the default action.
    /// Blank lines and lines starting with "#" or ";" are ignored.
    pub fn add_line(&mut self, line: &str, default_include: bool) -> Result<(), String> {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.trim().is_empty() || line.starts_with('#') || line.starts_with(';') {
            return Ok(());
        }
        for (prefix, include) in [("+ ", true), ("- ", false), ("include ", true), ("exclude ", false)] {
            if let Some(p) = line.strip_prefix(prefix) {
                return self.add(include, p);
            }
        }
        self.add(default_include, line)
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Makes patterns see `name/` in front of every path; see `prefix`.
    pub fn set_prefix(&mut self, name: &[u8]) {
        self.prefix = name.to_vec();
    }

    /// `rel` as patterns see it.
    fn full<'a>(&'a self, rel: &'a [u8]) -> std::borrow::Cow<'a, [u8]> {
        if self.prefix.is_empty() {
            rel.into()
        } else if rel.is_empty() {
            self.prefix[..].into()
        } else {
            [&self.prefix[..], b"/", rel].concat().into()
        }
    }

    fn check(&self, path: &[u8], is_dir: bool) -> bool {
        for r in &self.rules {
            if r.matches(path, is_dir) {
                return !r.include;
            }
        }
        false
    }

    fn check_path(&self, path: &[u8], is_dir: bool) -> bool {
        if path.is_empty() {
            return false;
        }
        for (i, &c) in path.iter().enumerate() {
            if c == b'/' && self.check(&path[..i], true) {
                return true;
            }
        }
        self.check(path, is_dir)
    }

    /// Whether `rel` (relative to the source directory, no leading slash)
    /// is excluded. Parent directories are not checked; see `excluded_path`.
    pub fn excluded(&self, rel: &[u8], is_dir: bool) -> bool {
        !self.rules.is_empty() && self.check(&self.full(rel), is_dir)
    }

    /// Like `excluded`, but also true when any parent directory is excluded,
    /// since a tree walk never descends into an excluded directory.
    pub fn excluded_path(&self, rel: &[u8], is_dir: bool) -> bool {
        !self.rules.is_empty() && self.check_path(&self.full(rel), is_dir)
    }

    /// Like `excluded_path` but only checks the parent directories.
    pub fn parent_excluded(&self, rel: &[u8]) -> bool {
        if self.rules.is_empty() {
            return false;
        }
        let f = self.full(rel);
        match f.iter().rposition(|&c| c == b'/') {
            Some(i) => self.check_path(&f[..i], true),
            None => false,
        }
    }
}

impl Rule {
    fn matches(&self, rel: &[u8], is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }
        if self.anchored {
            return wildmatch(&self.pat, rel);
        }
        if self.wild2 {
            return wildmatch(&self.pat, rel)
                || rel.iter().enumerate().any(|(i, &c)| c == b'/' && wildmatch(&self.pat, &rel[i + 1..]));
        }
        // Compare against the last `components` components of the path.
        let mut n = self.components;
        let mut start = 0;
        for i in (0..rel.len()).rev() {
            if rel[i] == b'/' {
                n -= 1;
                if n == 0 {
                    start = i + 1;
                    break;
                }
            }
        }
        wildmatch(&self.pat, &rel[start..])
    }
}

/// Shell-style matching where "*" and "?" don't match "/", "**" matches
/// anything, "[...]" is a character class and "\" escapes the next character.
pub fn wildmatch(p: &[u8], t: &[u8]) -> bool {
    let (mut pi, mut ti) = (0, 0);
    while pi < p.len() {
        match p[pi] {
            b'*' => {
                let mut np = pi;
                while np < p.len() && p[np] == b'*' {
                    np += 1;
                }
                let double = np - pi >= 2;
                let rest = &p[np..];
                if rest.is_empty() {
                    return double || !t[ti..].contains(&b'/');
                }
                let mut i = ti;
                loop {
                    if wildmatch(rest, &t[i..]) {
                        return true;
                    }
                    if i >= t.len() || (!double && t[i] == b'/') {
                        return false;
                    }
                    i += 1;
                }
            }
            b'?' => {
                if ti >= t.len() || t[ti] == b'/' {
                    return false;
                }
                ti += 1;
                pi += 1;
            }
            b'[' => match class(&p[pi..], t.get(ti).copied()) {
                Some((ok, len)) => {
                    if !ok {
                        return false;
                    }
                    ti += 1;
                    pi += len;
                }
                None => {
                    if t.get(ti) != Some(&b'[') {
                        return false;
                    }
                    ti += 1;
                    pi += 1;
                }
            },
            b'\\' if pi + 1 < p.len() => {
                if t.get(ti) != Some(&p[pi + 1]) {
                    return false;
                }
                ti += 1;
                pi += 2;
            }
            c => {
                if t.get(ti) != Some(&c) {
                    return false;
                }
                ti += 1;
                pi += 1;
            }
        }
    }
    ti == t.len()
}

/// Matches a bracket expression at the start of `p` against `c`. Returns
/// whether it matched and the expression's length, or None if `p` doesn't
/// start a complete bracket expression.
fn class(p: &[u8], c: Option<u8>) -> Option<(bool, usize)> {
    let mut i = 1;
    let negate = matches!(p.get(1), Some(b'!' | b'^'));
    if negate {
        i += 1;
    }
    let first = i;
    let mut hit = false;
    loop {
        let ch = *p.get(i)?;
        if ch == b']' && i > first {
            break;
        }
        let lo = if ch == b'\\' {
            i += 1;
            *p.get(i)?
        } else {
            ch
        };
        i += 1;
        let mut hi = lo;
        if p.get(i) == Some(&b'-') && p.get(i + 1).is_some_and(|&x| x != b']') {
            i += 1;
            if p[i] == b'\\' {
                i += 1;
            }
            hi = *p.get(i)?;
            i += 1;
        }
        if c.is_some_and(|c| lo <= c && c <= hi) {
            hit = true;
        }
    }
    let ok = c.is_some_and(|c| c != b'/') && hit != negate;
    Some((ok, i + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(rules: &[&str]) -> Filter {
        let mut f = Filter::default();
        for r in rules {
            f.add_line(r, false).unwrap();
        }
        f
    }

    #[test]
    fn wildcards() {
        assert!(wildmatch(b"*.txt", b"a.txt"));
        assert!(!wildmatch(b"*.txt", b"d/a.txt"));
        assert!(wildmatch(b"**.txt", b"d/a.txt"));
        assert!(wildmatch(b"a?c", b"abc"));
        assert!(!wildmatch(b"a?c", b"a/c"));
        assert!(wildmatch(b"[abc]x", b"bx"));
        assert!(!wildmatch(b"[!abc]x", b"bx"));
        assert!(wildmatch(b"[a-c]x", b"cx"));
        assert!(wildmatch(b"lost+found", b"lost+found"));
        assert!(wildmatch(b"\\*", b"*"));
        assert!(!wildmatch(b"\\*", b"a"));
        assert!(wildmatch(b"a/**/b", b"a/x/y/b"));
        assert!(wildmatch(b"[x", b"[x"));
    }

    #[test]
    fn anchored_contents() {
        // The pattern rsync users write to keep a directory but skip its contents.
        let f = f(&["- /home/*", "- /lost+found", "- /swap"]);
        assert!(!f.excluded(b"home", true));
        assert!(f.excluded(b"home/sen", true));
        assert!(f.excluded(b"home/.profile", false));
        assert!(f.excluded_path(b"home/sen/deep/file", false));
        assert!(!f.excluded(b"etc/home/sen", true));
        assert!(f.excluded(b"lost+found", true));
        assert!(!f.excluded(b"x/lost+found", true));
        assert!(f.excluded(b"swap", false));
    }

    #[test]
    fn unanchored() {
        let f = f(&["- cache", "- *.tmp", "- build/", "- a/b"]);
        assert!(f.excluded(b"x/y/cache", false));
        assert!(f.excluded(b"cache", true));
        assert!(f.excluded(b"q/w.tmp", false));
        assert!(f.excluded(b"src/build", true));
        assert!(!f.excluded(b"src/build", false));
        assert!(f.excluded(b"x/a/b", false));
        assert!(f.excluded(b"a/b", false));
        assert!(!f.excluded(b"x/za/b", false));
    }

    #[test]
    fn first_match_wins() {
        let f = f(&["+ /.drive-sync-test-1-*", "- *"]);
        assert!(!f.excluded(b".drive-sync-test-1-on-990", false));
        assert!(f.excluded(b".drive-sync-test-2-on-990", false));
        assert!(f.excluded(b"etc", true));
    }

    #[test]
    fn triple_star() {
        let f = f(&["+ /keep/***", "- *"]);
        assert!(!f.excluded(b"keep", true));
        assert!(!f.excluded(b"keep/a/b", false));
        assert!(f.excluded(b"other", true));
    }

    #[test]
    fn line_syntax() {
        let f = f(&["# comment", "", "include /a", "exclude /b", "/c"]);
        assert!(!f.excluded(b"a", false));
        assert!(f.excluded(b"b", false));
        assert!(f.excluded(b"c", false));
    }

    #[test]
    fn prefix() {
        // Source "/x/sen" without a trailing slash: patterns see "sen/...".
        let mut f = f(&["- /sen/cache", "- /other"]);
        f.set_prefix(b"sen");
        assert!(f.excluded(b"cache", true));
        assert!(!f.excluded(b"x/cache", true));
        assert!(f.parent_excluded(b"cache/a"));
        assert!(!f.excluded(b"", true));
        let mut g = super::Filter::default();
        g.add(false, "/sen").unwrap();
        g.set_prefix(b"sen");
        assert!(g.excluded_path(b"a/b", false));
        assert!(g.excluded_path(b"", true));
    }

    #[test]
    fn parents() {
        let f = f(&["- /var/log/*"]);
        assert!(f.parent_excluded(b"var/log/x/y"));
        assert!(!f.parent_excluded(b"var/log/x"));
        assert!(f.excluded_path(b"var/log/x", false));
        assert!(!f.parent_excluded(b"var/log"));
    }
}
