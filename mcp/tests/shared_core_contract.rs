#[path = "support/search_contract.rs"]
mod contract;

#[test]
fn preserves_mcp_search_contract() {
    let fixture = contract::Fixture::new().unwrap();
    let _keep_fixture = &fixture.root;
    let search = fixture.search().unwrap();
    let actual = contract::capture(&search).unwrap();
    contract::validate_contract(&actual);
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/search_contract.json")).unwrap();
    assert_eq!(actual, expected);

    // The full relevance order is strict above; pagination must select its exact
    // slice, including when the engine serves a cached ranked page.
    let mut request =
        otzaria_mcp::search::SearchRequest::new("שלום", otzaria_mcp::search::Mode::Exact);
    request.order = "relevance".into();
    request.limit = 2;
    request.offset = 1;
    let page = serde_json::to_value(search.search(&request).unwrap()).unwrap();
    assert_eq!(
        page["hits"].as_array().unwrap(),
        &actual["exact_relevance"]["response"]["hits"]
            .as_array()
            .unwrap()[1..3]
    );
}
