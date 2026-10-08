use super::*;

#[test]
fn tool_definition_shape() {
    let t = tool_definition();
    assert_eq!(t["type"], "function");
    assert_eq!(t["name"], TOOL_NAME);
    assert_eq!(t["parameters"]["required"], json!(["query"]));
}

#[test]
fn argument_parsing() {
    assert_eq!(
        parse_args(r#"{"query":" vacation ","top_k":3}"#),
        Some(SearchArgs {
            query: "vacation".to_owned(),
            top_k: Some(3)
        })
    );
    assert_eq!(
        parse_args(r#"{"query":"x","top_k":0}"#).unwrap().top_k,
        None
    );
    assert!(parse_args(r#"{"query":"  "}"#).is_none());
    assert!(parse_args(r#"{"q":"x"}"#).is_none());
    assert!(parse_args("not json").is_none());
}

#[test]
fn result_parsing_trims_and_limits() {
    let v = json!({"data": [
        {"filename": "a.pdf", "score": 0.9, "content": [{"type": "text", "text": "abcdefghij"}, {"type": "text", "text": "klm"}]},
        {"filename": "b.pdf", "content": [{"type": "text", "text": "second"}]},
        {"filename": "c.pdf", "content": []}
    ]});
    let chunks = parse_results(&v, 5, 2);
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].text, "abcde");
    assert_eq!(chunks[0].filename, "a.pdf");
    assert_eq!(chunks[0].score, Some(0.9));
    assert_eq!(chunks[1].text, "secon");
    assert!(parse_results(&json!({}), 5, 2).is_empty());
}

#[test]
fn outputs_are_json() {
    let out = results_output(&[Chunk {
        filename: "a".into(),
        text: "t".into(),
        score: None,
    }]);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["results"][0]["text"], "t");
    for s in [limit_output(), failure_output(), invalid_args_output()] {
        let v: Value = serde_json::from_str(&s).unwrap();
        assert!(v["error"].is_string());
    }
    assert!(limit_output().contains("limit"));
    assert!(failure_output().contains("fail"));
}
