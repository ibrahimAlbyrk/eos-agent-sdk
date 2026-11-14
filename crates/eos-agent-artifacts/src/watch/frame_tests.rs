use super::*;

fn row(id: &str, lane: Lane, text: &str) -> Row {
    Row {
        id: id.into(),
        account: "u_1".into(),
        lane,
        text: text.into(),
        access: None,
        by_artifact: false,
        sent_by_viewer: Some(true),
    }
}

fn mark_of(text: &str) -> String {
    let at = text.find("between the U").expect("fence named") + "between the ".len();
    text[at..at + 13].to_owned()
}

#[test]
fn a_thread_is_framed_with_its_anchor_and_heads() {
    let rows = vec![row("c1", Lane::Agent, "Earlier answer"), row("c2", Lane::Human, "[Make it\nblue]")];
    let anchor = Anchor { path: Some("#hero".into()), label: Some("Pricing".into()), ..Anchor::default() };
    let summons: HashSet<String> = ["c2".to_owned()].into();
    let snippet = Snippet { element: Some("<h1 id=\"hero\"> Plans".into()), inside: Vec::new() };
    let framed = frame(&Thread {
        rows: &rows,
        anchor: &anchor,
        trigger: Trigger::Fresh,
        summons: &summons,
        viewer: Some("u_1"),
        foreign: false,
        snippet: Some(&snippet),
    });
    let t = &framed.text;
    let m = mark_of(t);
    assert!(t.starts_with(
        "A human activated you on a comment thread of an artifact you published, and a new human comment arrived."
    ));
    assert!(t.contains("Every row whose head is \"[human, sent to you]\" is a comment sent to Claude"));
    assert!(t.contains(" A line starting \"[location]\" says where on the page"));
    assert!(t.contains(" Lines starting \"[anchored at]\" and \"[anchored element]\":"));
    let body = format!(
        "<{m}>\n[location] Pricing\n[anchored at] #hero\n[anchored element] <h1 id=\"hero\"> Plans\n[assistant]\n{m}| \
         Earlier answer\n[human, sent to you]\n{m}| [Make it\n{m}| blue]\n</{m}>"
    );
    assert!(t.ends_with(&body), "{t}");
    assert!(!t.contains("<artifact-content-authored-by-others/>"));
}

#[test]
fn older_comments_are_elided_past_the_budget() {
    let mut rows: Vec<Row> = (0..40).map(|i| row(&format!("c{i}"), Lane::Human, &"x".repeat(1000))).collect();
    rows.push(row("last", Lane::Human, "newest"));
    let anchor = Anchor::default();
    let summons = HashSet::new();
    let framed = frame(&Thread {
        rows: &rows,
        anchor: &anchor,
        trigger: Trigger::Activation,
        summons: &summons,
        viewer: None,
        foreign: true,
        snippet: None,
    });
    let t = &framed.text;
    let m = mark_of(t);
    assert!(t.starts_with("<artifact-content-authored-by-others/>\n"));
    assert!(t.contains(&format!("<{m}>\n[25 earlier comments elided]\n[human]\n")), "{t}");
    assert!(t.ends_with(&format!("[human]\n{m}| newest\n</{m}>")));
}

#[test]
fn mentions_and_brackets_are_read_as_the_composer_reads_them() {
    let access: HashMap<String, String> =
        [("0f8e2a9c-1b3d-4c5e-8f70-112233445566".to_owned(), "editor".to_owned())].into();
    let text = "hi <mention:claude> and <mention:0f8e2a9c-1b3d-4c5e-8f70-112233445566> <mention:x";
    assert_eq!(mentions(text, None, &access), "hi @Claude and @editor 0f8e2a9c <mention:x");
    assert_eq!(fence("plain", "M", "", false), "plain");
    assert_eq!(fence("(aside)", "M", "", false), "M| (aside)");
}
