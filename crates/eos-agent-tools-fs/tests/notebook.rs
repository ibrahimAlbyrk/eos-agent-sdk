mod common;

use common::{Env, err, ok};
use serde_json::{Value, json};

fn notebook() -> String {
    // Jupyter's own layout: indent 1, keys in file order (not sorted here on purpose).
    "{\n \"cells\": [\n  {\n   \"cell_type\": \"code\",\n   \"id\": \"aaa\",\n   \"source\": [\"x = 1\"],\n   \"metadata\": {},\n   \"execution_count\": 3,\n   \"outputs\": [{\"output_type\": \"stream\", \"text\": \"1\"}]\n  },\n  {\n   \"cell_type\": \"markdown\",\n   \"id\": \"bbb\",\n   \"source\": \"# T\",\n   \"metadata\": {}\n  }\n ],\n \"metadata\": {\"zeta\": 1, \"alpha\": 2},\n \"nbformat\": 4,\n \"nbformat_minor\": 5\n}".into()
}

fn cells(env: &Env) -> Vec<Value> {
    let nb: Value = serde_json::from_str(&env.read("n.ipynb")).expect("json");
    nb["cells"].as_array().expect("cells").clone()
}

#[tokio::test]
async fn replace_resets_outputs_and_keeps_layout() {
    let env = Env::new();
    env.write("n.ipynb", notebook());
    let input = json!({"notebook_path": env.p("n.ipynb"), "cell_id": "aaa", "new_source": "x = 2"});
    let msg = err(&env.notebook(input.clone()).await);
    assert_eq!(msg, "File has not been read yet. Read it first before writing to it.");
    env.read_file("n.ipynb").await;
    assert_eq!(ok(&env.notebook(input).await), "Updated cell aaa with x = 2");
    let text = env.read("n.ipynb");
    assert!(text.contains("\"metadata\": {\n  \"zeta\": 1,\n  \"alpha\": 2\n }"), "{text}");
    let c = cells(&env);
    assert_eq!(c[0]["source"], "x = 2");
    assert_eq!(c[0]["execution_count"], Value::Null);
    assert_eq!(c[0]["outputs"], json!([]));
}

#[tokio::test]
async fn insert_and_delete() {
    let env = Env::new();
    env.write("n.ipynb", notebook());
    env.read_file("n.ipynb").await;
    let out = ok(&env
        .notebook(json!({"notebook_path": env.p("n.ipynb"), "cell_id": "aaa", "new_source": "## mid", "cell_type": "markdown", "edit_mode": "insert"}))
        .await);
    assert!(out.starts_with("Inserted cell ") && out.ends_with(" with ## mid"), "{out}");
    let c = cells(&env);
    assert_eq!(c.len(), 3);
    assert_eq!(c[1]["source"], "## mid");
    assert_eq!(c[1]["id"].as_str().map(str::len), Some(8));

    let msg =
        err(&env.notebook(json!({"notebook_path": env.p("n.ipynb"), "new_source": "y", "edit_mode": "insert"})).await);
    assert_eq!(msg, "Cell type is required when using edit_mode=insert.");
    ok(&env.notebook(json!({"notebook_path": env.p("n.ipynb"), "new_source": "import os", "cell_type": "code", "edit_mode": "insert"})).await);
    assert_eq!(cells(&env)[0]["source"], "import os");

    let out = ok(&env
        .notebook(json!({"notebook_path": env.p("n.ipynb"), "cell_id": "bbb", "new_source": "", "edit_mode": "delete"}))
        .await);
    assert_eq!(out, "Deleted cell bbb");
    assert_eq!(cells(&env).len(), 3);
}

#[tokio::test]
async fn cells_by_index_label_and_missing_ids() {
    let env = Env::new();
    env.write("n.ipynb", notebook());
    env.read_file("n.ipynb").await;
    ok(&env.notebook(json!({"notebook_path": env.p("n.ipynb"), "cell_id": "cell-1", "new_source": "# New"})).await);
    assert_eq!(cells(&env)[1]["source"], "# New");
    let msg = err(&env.notebook(json!({"notebook_path": env.p("n.ipynb"), "cell_id": "zzz", "new_source": "x"})).await);
    assert_eq!(msg, "Cell with ID \"zzz\" not found in notebook.");
    let msg = err(&env.notebook(json!({"notebook_path": env.p("n.ipynb"), "new_source": "x"})).await);
    assert_eq!(msg, "Cell ID must be specified when not inserting a new cell.");
    env.write("n.txt", "x");
    let msg = err(&env.notebook(json!({"notebook_path": env.p("n.txt"), "cell_id": "a", "new_source": "x"})).await);
    assert!(msg.starts_with("File must be a Jupyter notebook (.ipynb file)."));
}
