use super::*;

fn one(html: &str, path: &str) -> Option<String> {
    snippet(html, path, &[]).map(|(s, _)| s)
}

#[test]
fn quotes_start_tag_and_collapsed_text() {
    let html = r#"<body><div id="a" class="x  y"><p>Hello <b>big</b>   world</p></div></body>"#;
    assert_eq!(one(html, "#a").as_deref(), Some(r#"<div id="a" class="x y"> Hello big world"#));
}

#[test]
fn follows_id_then_nth_of_type() {
    let html = r#"<section id="root"><div>one</div><span>s</span><div><p>first</p><p>second</p></div></section>"#;
    let got = one(html, "#root > div:nth-of-type(2) > p:nth-of-type(1)");
    assert_eq!(got.as_deref(), Some("<p> first"));
}

#[test]
fn data_id_path_and_kids() {
    let html = r#"<ul data-id="list"><li>a<li>b</ul>"#;
    assert_eq!(one(html, r#"[data-id="list"] > li:nth-of-type(2)"#).as_deref(), Some("<li> b"));
    let (main, kids) = snippet(html, r#"[data-id="list"]"#, &[1, 0, 1, 5]).expect("anchored");
    assert_eq!(main, r#"<ul data-id="list"> a b"#);
    assert_eq!(kids, vec!["<li> b".to_string(), "<li> a".to_string()]);
}

#[test]
fn html_head_and_body_are_implied() {
    let html = "<!doctype html><title>T</title><!-- c --><main>Body text</main>";
    let main = "html:nth-of-type(1) > body:nth-of-type(1) > main:nth-of-type(1)";
    assert_eq!(one(html, main).as_deref(), Some("<main> Body text"));
    let title = "html:nth-of-type(1) > head:nth-of-type(1) > title:nth-of-type(1)";
    assert_eq!(one(html, title).as_deref(), Some("<title> T"));
}

#[test]
fn long_text_is_cut_at_120() {
    let body = "word ".repeat(40);
    let html = format!(r#"<p id="t">{body}</p>"#);
    assert_eq!(one(&html, "#t"), Some(format!(r#"<p id="t"> {}…"#, &body[..120])));
}

#[test]
fn long_start_tag_is_cut_at_160() {
    let open = format!(r#"<div id="z" title="{}">"#, "x".repeat(300));
    let html = format!("{open}hi</div>");
    assert_eq!(one(&html, "#z"), Some(format!("{}… hi", &open[..160])));
}

#[test]
fn script_and_style_are_left_out() {
    let html = r#"<div id="s">a<script>var x = "<b>";</script><style>p{}</style>b</div>"#;
    assert_eq!(one(html, "#s").as_deref(), Some(r#"<div id="s"> ab"#));
    assert_eq!(one(html, "#s > script:nth-of-type(1)").as_deref(), Some("<script>"));
}

#[test]
fn ambiguous_or_missing_paths_give_none() {
    let html = "<div>x</div><section><div>y</div></section>";
    assert_eq!(one(html, "div:nth-of-type(1)"), None);
    assert_eq!(one(html, "body:nth-of-type(1) > div:nth-of-type(1)").as_deref(), Some("<div> x"));
    assert_eq!(one(html, "#nope"), None);
}

#[test]
fn attribute_entities_are_decoded_and_quotes_reescaped() {
    let html = r#"<a id="q" title="say &quot;hi&quot; &amp; go" href=/x?a=1&amp;b=2>Tom &amp; Jerry &lt;3 &#x41;</a>"#;
    let want = r#"<a id="q" title="say &quot;hi&quot; & go" href="/x?a=1&b=2"> Tom & Jerry <3 A"#;
    assert_eq!(one(html, "#q").as_deref(), Some(want));
}

#[test]
fn bad_paths_give_none() {
    let html = r#"<div id="a"><div id="b"></div></div>"#;
    for path in ["", "div", "#a > #b", "div:nth-of-type(0)", "Div:nth-of-type(1)", "#1a", "div:nth-of-type(1) >"] {
        assert_eq!(one(html, path), None, "{path}");
    }
    assert_eq!(one(html, &["div:nth-of-type(1)"; 11].join(" > ")), None);
}

#[test]
fn implied_end_tags_and_tbody() {
    let html = r#"<p id="p1">one<div>two</div><table id="t"><tr><td>1<td>2</table>"#;
    assert_eq!(one(html, "#p1").as_deref(), Some(r#"<p id="p1"> one"#));
    let cell = "#t > tbody:nth-of-type(1) > tr:nth-of-type(1) > td:nth-of-type(2)";
    assert_eq!(one(html, cell).as_deref(), Some("<td> 2"));
}

#[test]
fn oversized_or_too_deep_pages_give_none() {
    let big = format!(r#"<p id="a">{}</p>"#, "x".repeat(PAGE_MAX));
    assert_eq!(one(&big, "#a"), None);
    assert_eq!(one(&format!(r#"<div id="a">{}"#, "<div>".repeat(600)), "#a"), None);
    let ok = format!(r#"<div id="a">{}"#, "<span>".repeat(100));
    assert_eq!(one(&ok, "#a").as_deref(), Some(r#"<div id="a">"#));
}
