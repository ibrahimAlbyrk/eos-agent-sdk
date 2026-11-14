//! The element a comment thread is anchored to, quoted from the page source.
//!
//! Claude Code parses the page with parse5 and quotes the element a path
//! names (`Mn`). Here a small tolerant tree builder keeps the parse5 shapes
//! those paths depend on: html/head/body always exist, `p`/`li`/`dd`/`dt`/
//! `option`/table cells close implicitly, tables get their `tbody`, and a
//! template's content is not part of the tree.

use std::collections::{HashMap, HashSet};

const TAG_MAX: usize = 160;
const TEXT_MAX: usize = 120;
const PAGE_MAX: usize = 2_000_000;
const WALK_MAX: usize = 2000;
const KIDS_MAX: usize = 256;
const DEPTH_MAX: usize = 512;

// Tag lists, space separated (see `has`).
const OPAQUE: &str = "script style template noscript";
const VOID: &str = "area base br col embed hr img input link meta source track wbr param keygen";
const RAW: &str = "script style textarea title xmp iframe noembed noframes noscript";
const HEAD_TAGS: &str = "meta link title style script base noscript template";
const CLOSES_P: &str = "address article aside blockquote details div dl fieldset figcaption figure footer form \
    h1 h2 h3 h4 h5 h6 header hgroup hr main menu nav ol p pre section table ul li dd dt";
const SPECIAL: &str = "address applet area article aside base blockquote body br button caption center col \
    colgroup dd details dir div dl dt embed fieldset figcaption figure footer form h1 h2 h3 h4 h5 h6 head header \
    hgroup hr html iframe img input li link listing main marquee menu meta nav noembed noframes noscript object ol \
    p param plaintext pre script section select source style summary table tbody td template textarea tfoot th \
    thead title tr track ul wbr xmp";
const SCOPE: &str = "html table td th caption template applet object marquee";
const TABLE_PARTS: &str = "table tbody thead tfoot tr td th caption";
/// SVG tag and attribute names parse5 gives back their camel case.
const SVG_CASE: &str = "clipPath linearGradient radialGradient foreignObject textPath feGaussianBlur feOffset \
    feBlend feColorMatrix feComposite feFlood feMerge feMergeNode feDropShadow viewBox preserveAspectRatio \
    gradientUnits gradientTransform patternUnits patternTransform clipPathUnits maskUnits stdDeviation markerWidth \
    markerHeight refX refY pathLength textLength attributeName repeatCount keyTimes calcMode spreadMethod \
    startOffset filterUnits";
/// Named character references: the name, then the char it stands for.
const NAMED: &str = "amp& lt< gt> quot\" apos' nbsp\u{a0} copy© reg® trade™ hellip… mdash— ndash– lsquo‘ rsquo’ \
    ldquo“ rdquo” laquo« raquo» middot· bull• times× euro€ deg° sect§";

fn has(list: &str, tag: &str) -> bool {
    list.split_ascii_whitespace().any(|t| t == tag)
}

/// The snippet of the element at `path` and of its children `kids` (by
/// index among its element children), as Claude Code's `Mn` quotes them.
pub(super) fn snippet(html: &str, path: &str, kids: &[usize]) -> Option<(String, Vec<String>)> {
    if html.len() > PAGE_MAX && html.chars().map(char::len_utf16).sum::<usize>() > PAGE_MAX {
        return None;
    }
    let segs = parse_path(path)?;
    let tree = build(html)?;
    let el = tree.find_one(&segs)?;
    let children: Vec<usize> = tree.nodes[el].kids.iter().copied().filter(|&k| tree.is_elem(k)).collect();
    let mut seen = HashSet::new();
    let quoted = kids
        .iter()
        .filter(|&&k| seen.insert(k))
        .take(KIDS_MAX)
        .filter_map(|&k| children.get(k).map(|&c| tree.quote(c)))
        .collect();
    Some((tree.quote(el), quoted))
}

#[derive(Debug)]
enum Seg {
    Id(String),
    DataId(String),
    Tag(String, usize),
}

fn parse_path(path: &str) -> Option<Vec<Seg>> {
    let parts: Vec<&str> = path.trim_matches(is_ws).split(" > ").collect();
    if parts.len() > 10 {
        return None;
    }
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    let mut segs = Vec::with_capacity(parts.len());
    for (i, part) in parts.into_iter().enumerate() {
        if i == 0 {
            if let Some(id) = part.strip_prefix('#') {
                let first = id.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_' || c == '-');
                if first && id.len() <= 32 && id.chars().all(word) {
                    segs.push(Seg::Id(id.to_string()));
                    continue;
                }
            }
            let data = part.strip_prefix("[data-id=\"").and_then(|r| r.strip_suffix("\"]"));
            if let Some(d) = data.filter(|d| (1..=64).contains(&d.len()) && d.chars().all(word)) {
                segs.push(Seg::DataId(d.to_string()));
                continue;
            }
        }
        let (tag, rest) = part.split_once(":nth-of-type(")?;
        let n = rest.strip_suffix(')')?;
        let tag_ok = tag.starts_with(|c: char| c.is_ascii_lowercase())
            && tag.len() <= 24
            && tag.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        let n_ok = (1..=4).contains(&n.len()) && !n.starts_with('0') && n.chars().all(|c| c.is_ascii_digit());
        if !tag_ok || !n_ok {
            return None;
        }
        segs.push(Seg::Tag(tag.to_string(), n.parse().ok()?));
    }
    Some(segs)
}

#[derive(Debug)]
enum Kind {
    Doc,
    Elem(String, Vec<(String, String)>),
    Text(String),
    Comment,
}

#[derive(Debug)]
struct Node {
    parent: usize,
    kind: Kind,
    kids: Vec<usize>,
}

const HTML: usize = 1;
const HEAD: usize = 2;
const BODY: usize = 3;

/// The document (node 0) and the stack of open elements; `in_body` is false
/// until the first content that belongs in the body.
#[derive(Debug)]
struct Tree {
    nodes: Vec<Node>,
    stack: Vec<usize>,
    in_body: bool,
    deep: bool,
}

fn elem(tag: &str) -> Kind {
    Kind::Elem(tag.to_string(), Vec::new())
}

impl Tree {
    fn new() -> Self {
        let doc = Node { parent: 0, kind: Kind::Doc, kids: Vec::new() };
        let mut t = Tree { nodes: vec![doc], stack: vec![HTML, HEAD], in_body: false, deep: false };
        t.append(0, elem("html"));
        t.append(HTML, elem("head"));
        t.append(HTML, elem("body"));
        t
    }

    fn tag(&self, id: usize) -> &str {
        match &self.nodes[id].kind {
            Kind::Elem(t, _) => t,
            _ => "",
        }
    }

    fn is_elem(&self, id: usize) -> bool {
        matches!(self.nodes[id].kind, Kind::Elem(..))
    }

    fn cur(&self) -> usize {
        self.stack.last().copied().unwrap_or(HTML)
    }

    /// A template's content lives outside the tree, as in parse5.
    fn append(&mut self, parent: usize, kind: Kind) -> usize {
        let id = self.nodes.len();
        self.nodes.push(Node { parent, kind, kids: Vec::new() });
        if self.tag(parent) != "template" {
            self.nodes[parent].kids.push(id);
        }
        id
    }

    fn add_text(&mut self, parent: usize, s: String) {
        if s.is_empty() {
            return;
        }
        if let Some(&last) = self.nodes[parent].kids.last()
            && let Kind::Text(t) = &mut self.nodes[last].kind
        {
            t.push_str(&s);
            return;
        }
        self.append(parent, Kind::Text(s));
    }

    fn text(&mut self, s: String) {
        if !self.in_body {
            if s.chars().all(is_ws) {
                return;
            }
            self.enter_body();
        }
        self.add_text(self.cur(), s);
    }

    fn enter_body(&mut self) {
        if !self.in_body {
            self.in_body = true;
            self.stack = vec![HTML, BODY];
        }
    }

    fn merge(&mut self, id: usize, attrs: Vec<(String, String)>) {
        if let Kind::Elem(_, have) = &mut self.nodes[id].kind {
            for (k, v) in attrs {
                if !have.iter().any(|(h, _)| *h == k) {
                    have.push((k, v));
                }
            }
        }
    }

    /// The stack index of the nearest open element `want` accepts, unless
    /// one `stop` accepts comes first.
    fn find(&self, stop: impl Fn(&str) -> bool, want: impl Fn(&str) -> bool) -> Option<usize> {
        for i in (0..self.stack.len()).rev() {
            let t = self.tag(self.stack[i]);
            if want(t) {
                return Some(i);
            }
            if stop(t) {
                return None;
            }
        }
        None
    }

    fn p_in_scope(&self) -> Option<usize> {
        self.find(|t| has(SCOPE, t) || t == "button", |t| t == "p")
    }

    fn close_item(&mut self, names: &str) {
        let stop = |t: &str| has(SPECIAL, t) && !matches!(t, "address" | "div" | "p");
        if let Some(j) = self.find(stop, |t| has(names, t)) {
            self.stack.truncate(j);
        }
    }

    fn start(&mut self, tag: String, attrs: Vec<(String, String)>, closed: bool) -> Option<usize> {
        match tag.as_str() {
            "html" => self.merge(HTML, attrs),
            "body" => {
                self.merge(BODY, attrs);
                self.enter_body();
            }
            "head" if !self.in_body => self.merge(HEAD, attrs),
            "head" => {}
            t if !self.in_body && has(HEAD_TAGS, t) => return self.insert(tag, attrs, closed),
            _ => return self.start_in_body(tag, attrs, closed),
        }
        None
    }

    fn start_in_body(&mut self, tag: String, attrs: Vec<(String, String)>, closed: bool) -> Option<usize> {
        self.enter_body();
        let outer = |t: &str| matches!(t, "html" | "template");
        match tag.as_str() {
            "li" => self.close_item("li"),
            "dd" | "dt" => self.close_item("dd dt"),
            "option" | "optgroup" if self.tag(self.cur()) == "option" => {
                self.stack.pop();
            }
            "tbody" | "thead" | "tfoot" | "caption" | "colgroup" => {
                let j = self.find(outer, |t| t == "table")?;
                self.stack.truncate(j + 1);
            }
            "tr" | "td" | "th" => {
                let cell = tag != "tr";
                let j = self.find(outer, |t| has("table tbody thead tfoot", t) || (cell && t == "tr"))?;
                self.stack.truncate(j + 1);
                if self.tag(self.cur()) == "table" {
                    self.insert("tbody".into(), Vec::new(), false);
                }
                if cell && self.tag(self.cur()) != "tr" {
                    self.insert("tr".into(), Vec::new(), false);
                }
            }
            _ => {}
        }
        if has(CLOSES_P, &tag)
            && let Some(j) = self.p_in_scope()
        {
            self.stack.truncate(j);
        }
        if is_heading(&tag) && is_heading(self.tag(self.cur())) {
            self.stack.pop();
        }
        self.insert(tag, attrs, closed)
    }

    fn insert(&mut self, mut tag: String, mut attrs: Vec<(String, String)>, closed: bool) -> Option<usize> {
        let foreign =
            matches!(tag.as_str(), "svg" | "math") || self.stack.iter().any(|&i| matches!(self.tag(i), "svg" | "math"));
        if foreign {
            svg_case(&mut tag);
            attrs.iter_mut().for_each(|(k, _)| svg_case(k));
        }
        let void = has(VOID, &tag) || (foreign && closed);
        let id = self.append(self.cur(), Kind::Elem(tag, attrs));
        if !void {
            self.stack.push(id);
            self.deep |= self.stack.len() > DEPTH_MAX;
        }
        Some(id)
    }

    fn end(&mut self, tag: &str) {
        match tag {
            "html" | "body" | "head" => return,
            "br" => {
                self.enter_body();
                self.insert("br".into(), Vec::new(), false);
                return;
            }
            "p" if self.in_body && self.p_in_scope().is_none() => {
                self.append(self.cur(), elem("p"));
                return;
            }
            _ => {}
        }
        let found = if has(TABLE_PARTS, tag) {
            self.find(|t| matches!(t, "html" | "template") || (t == "table" && tag != "table"), |t| t == tag)
        } else if has(SPECIAL, tag) {
            let list = tag == "li";
            self.find(|t| has(SCOPE, t) || (list && matches!(t, "ol" | "ul")), |t| t == tag)
        } else {
            self.find(|t| has(SPECIAL, t), |t| t == tag)
        };
        if let Some(j) = found.filter(|&j| j >= 2) {
            self.stack.truncate(j);
        }
    }

    /// The one element `segs` names, walking up from it through its
    /// ancestors; `None` when no element or more than one matches.
    fn find_one(&self, segs: &[Seg]) -> Option<usize> {
        let mut nth = vec![0usize; self.nodes.len()];
        let mut todo = vec![0usize];
        let mut hit = None;
        while let Some(id) = todo.pop() {
            let mut count: HashMap<&str, usize> = HashMap::new();
            for &k in &self.nodes[id].kids {
                if let Kind::Elem(tag, _) = &self.nodes[k].kind {
                    let n = count.entry(tag).or_default();
                    *n += 1;
                    nth[k] = *n;
                    todo.push(k);
                }
            }
            if id != 0 && self.matches(&nth, id, segs) {
                if hit.is_some() {
                    return None;
                }
                hit = Some(id);
            }
        }
        hit
    }

    fn matches(&self, nth: &[usize], id: usize, segs: &[Seg]) -> bool {
        let mut cur = id;
        for seg in segs.iter().rev() {
            let Kind::Elem(tag, attrs) = &self.nodes[cur].kind else { return false };
            let attr = |name: &str| attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v);
            let ok = match seg {
                Seg::Id(v) => attr("id") == Some(v),
                Seg::DataId(v) => attr("data-id") == Some(v),
                Seg::Tag(t, n) => tag == t && nth[cur] == *n,
            };
            if !ok {
                return false;
            }
            cur = self.nodes[cur].parent;
        }
        true
    }

    /// Claude Code's `ur`: the start tag, then the element's text.
    fn quote(&self, id: usize) -> String {
        let Kind::Elem(tag, attrs) = &self.nodes[id].kind else { return String::new() };
        let mut open = format!("<{tag}");
        for (k, v) in attrs {
            let mut value = String::new();
            push_collapsed(&mut value, &mut 0, v);
            open.push_str(&format!(" {k}=\"{}\"", value.replace('"', "&quot;")));
        }
        open.push('>');
        let (mut head, cut_tag) = cut(&open, TAG_MAX);
        if cut_tag {
            head.push('…');
        }
        if has(OPAQUE, tag) {
            return head;
        }
        let (mut text, mut len, mut more, mut steps) = (String::new(), 0usize, false, 0usize);
        let mut todo: Vec<usize> = self.nodes[id].kids.iter().rev().copied().collect();
        loop {
            steps += 1;
            if steps > WALK_MAX || len > TEXT_MAX + 2 {
                more = steps > WALK_MAX || !todo.is_empty();
                break;
            }
            let Some(n) = todo.pop() else { break };
            match &self.nodes[n].kind {
                Kind::Text(s) => push_collapsed(&mut text, &mut len, s),
                Kind::Elem(t, _) if !has(OPAQUE, t) => {
                    push_collapsed(&mut text, &mut len, " ");
                    todo.extend(self.nodes[n].kids.iter().rev());
                }
                _ => {}
            }
        }
        let (body, cut_text) = cut(text.trim_matches(is_ws), TEXT_MAX);
        if body.is_empty() {
            return head;
        }
        format!("{head} {body}{}", if more || cut_text { "…" } else { "" })
    }
}

fn is_heading(t: &str) -> bool {
    matches!(t, "h1" | "h2" | "h3" | "h4" | "h5" | "h6")
}

fn svg_case(name: &mut String) {
    if let Some(c) = SVG_CASE.split_ascii_whitespace().find(|c| c.eq_ignore_ascii_case(name)) {
        *name = c.to_string();
    }
}

/// JavaScript's `\s`, close enough.
fn is_ws(c: char) -> bool {
    c.is_whitespace() || c == '\u{feff}'
}

/// Appends `s` to the already collapsed `out`, collapsing whitespace runs
/// to one space (`xn(out + s)`); `len` counts the chars of `out`.
fn push_collapsed(out: &mut String, len: &mut usize, s: &str) {
    for c in s.chars() {
        if !is_ws(c) {
            out.push(c);
        } else if out.ends_with(' ') {
            continue;
        } else {
            out.push(' ');
        }
        *len += 1;
    }
}

/// At most `n` chars of `s`, and whether anything was cut.
fn cut(s: &str, n: usize) -> (String, bool) {
    match s.char_indices().nth(n) {
        Some((i, _)) => (s[..i].to_string(), true),
        None => (s.to_string(), false),
    }
}

fn build(src: &str) -> Option<Tree> {
    let b = src.as_bytes();
    let mut t = Tree::new();
    let mut i = 0;
    let skip_to_gt = |i: usize| src[i..].find('>').map_or(b.len(), |k| i + k + 1);
    while i < b.len() && !t.deep {
        if b[i] != b'<' {
            let j = src[i..].find('<').map_or(b.len(), |k| i + k);
            t.text(decode(&src[i..j]));
            i = j;
            continue;
        }
        let next = b.get(i + 1).copied().unwrap_or(0);
        if next.is_ascii_alphabetic() {
            let Some((tag, attrs, closed, end)) = read_tag(src, i + 1) else { break };
            i = end;
            let raw = has(RAW, &tag).then(|| tag.clone());
            let id = t.start(tag, attrs, closed);
            // Raw text runs to its end tag, which the next round pops.
            if let (Some(name), Some(id)) = (raw, id.filter(|&id| t.cur() == id)) {
                let stop = raw_end(src, i, &name);
                let text = &src[i..stop];
                let text = if matches!(name.as_str(), "textarea" | "title") { decode(text) } else { text.to_string() };
                t.add_text(id, text);
                i = stop;
            }
        } else if next == b'/' {
            if !b.get(i + 2).is_some_and(u8::is_ascii_alphabetic) {
                i = skip_to_gt(i);
                continue;
            }
            let Some((tag, _, _, end)) = read_tag(src, i + 2) else { break };
            i = end;
            t.end(&tag);
        } else if let Some(rest) = src[i..].strip_prefix("<!--") {
            let from = i + 4;
            i = match rest.find("-->") {
                _ if rest.starts_with('>') => from + 1,
                _ if rest.starts_with("->") => from + 2,
                Some(k) => from + k + 3,
                None => b.len(),
            };
            t.append(t.cur(), Kind::Comment);
        } else if next == b'!' || next == b'?' {
            i = skip_to_gt(i);
        } else {
            t.text("<".to_string());
            i += 1;
        }
    }
    (!t.deep).then_some(t)
}

/// Where a raw-text element's content ends: at its end tag, or the page's.
fn raw_end(src: &str, from: usize, name: &str) -> usize {
    let b = src.as_bytes();
    let mut k = from;
    while let Some(off) = src[k..].find("</") {
        let p = k + off;
        let q = p + 2 + name.len();
        let named = src.get(p + 2..q).is_some_and(|s| s.eq_ignore_ascii_case(name));
        if named && b.get(q).is_none_or(|&c| c == b'>' || c == b'/' || is_space(c)) {
            return p;
        }
        k = p + 2;
    }
    src.len()
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c')
}

/// The first index from `i` on whose byte `stop` accepts, or the end.
fn scan(b: &[u8], i: usize, stop: impl Fn(u8) -> bool) -> usize {
    b[i.min(b.len())..].iter().position(|&c| stop(c)).map_or(b.len(), |k| i + k)
}

type Tag = (String, Vec<(String, String)>, bool, usize);

/// The tag whose name starts at `i`: name, attributes, self-closing, and
/// where it ends. `None` when the page ends inside it (parse5 drops it).
fn read_tag(src: &str, mut i: usize) -> Option<Tag> {
    let b = src.as_bytes();
    let delim = |c: u8| is_space(c) || c == b'/' || c == b'>';
    let skip_space = |i: usize| scan(b, i, |c| !is_space(c));
    let start = i;
    i = scan(b, i, delim);
    let name = src[start..i].to_ascii_lowercase();
    let mut attrs: Vec<(String, String)> = Vec::new();
    loop {
        i = skip_space(i);
        match *b.get(i)? {
            b'>' => return Some((name, attrs, false, i + 1)),
            b'/' if b.get(i + 1) == Some(&b'>') => return Some((name, attrs, true, i + 2)),
            b'/' => {
                i += 1;
                continue;
            }
            _ => {}
        }
        let key_start = i;
        i = scan(b, i + 1, |c| delim(c) || c == b'=');
        let key = src[key_start..i].to_ascii_lowercase();
        i = skip_space(i);
        let mut value = String::new();
        if b.get(i) == Some(&b'=') {
            i = skip_space(i + 1);
            if let Some(&q @ (b'"' | b'\'')) = b.get(i) {
                let end = i + 1 + src[i + 1..].find(q as char)?;
                value = decode(&src[i + 1..end]);
                i = end + 1;
            } else {
                let v = i;
                i = scan(b, i, |c| is_space(c) || c == b'>');
                value = decode(&src[v..i]);
            }
        }
        if !attrs.iter().any(|(k, _)| *k == key) {
            attrs.push((key, value));
        }
    }
}

/// Decodes character references: numeric ones and the common named ones.
fn decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        rest = &rest[p..];
        let (c, used) = char_ref(rest).unwrap_or(('&', 1));
        out.push(c);
        rest = &rest[used..];
    }
    out.push_str(rest);
    out
}

/// The char a reference at the start of `s` stands for, and its length.
fn char_ref(s: &str) -> Option<(char, usize)> {
    let b = s.as_bytes();
    if b.get(1) == Some(&b'#') {
        let hex = matches!(b.get(2), Some(b'x' | b'X'));
        let from = if hex { 3 } else { 2 };
        let digits = s[from..].find(|c: char| !(if hex { c.is_ascii_hexdigit() } else { c.is_ascii_digit() }));
        let i = from + digits.unwrap_or(s.len() - from);
        if i == from {
            return None;
        }
        let n = u32::from_str_radix(&s[from..i], if hex { 16 } else { 10 }).unwrap_or(u32::MAX);
        let c = char::from_u32(n).filter(|&c| c != '\0').unwrap_or('\u{fffd}');
        return Some((c, if b.get(i) == Some(&b';') { i + 1 } else { i }));
    }
    let end = 1 + s[1..].find(|c: char| !c.is_ascii_alphanumeric())?;
    if b.get(end) != Some(&b';') {
        return None;
    }
    let entry = NAMED.split(' ').find(|e| e.strip_prefix(&s[1..end]).is_some_and(|v| v.chars().count() == 1))?;
    Some((entry.chars().last()?, end + 1))
}

#[cfg(test)]
#[path = "anchor_tests.rs"]
mod tests;
