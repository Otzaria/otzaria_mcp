//! Reproducible JSON used to compare the pre-migration MCP with the shared core.
//! `--create ROOT` leaves a fixture for an independently built engine adapter.
//! `--open ROOT` opens an existing fixture without indexing or metadata changes.
#[path = "../tests/support/search_contract.rs"]
mod contract;

fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let fixture;
    let search = if args.len() == 3 {
        let root = std::path::Path::new(&args[2]);
        if args[1] == "--create" {
            std::fs::create_dir_all(root)?;
            contract::build_fixture(&root.join("seforim.db"), &root.join("index"))?;
        } else {
            anyhow::ensure!(args[1] == "--open", "expected --create or --open");
        }
        otzaria_mcp::search::Search::open(
            &root.join("index"),
            None,
            &root.join("seforim.db"),
            None,
        )?
    } else {
        fixture = contract::Fixture::new()?;
        let _keep_fixture = &fixture.root;
        fixture.search()?
    };
    let value = contract::capture(&search)?;
    contract::validate_contract(&value);
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}
