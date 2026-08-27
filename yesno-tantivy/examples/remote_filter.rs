//! Filter Tantivy's e-commerce warmer fixture through a remote yesno server.
//!
//! The inventory key should contain olive oil (`323423`) and gloves (`3966623`).
//! The delivery key should contain olive oil (`323423`) and sneakers (`23222`).
//! This leaves olive oil as the only `cooking` result satisfying both cohorts.
//!
//! Run with:
//!
//! ```text
//! cargo run -p yesno-tantivy --features flight --example remote_filter -- \
//!   http://127.0.0.1:50051 100 200
//! ```
//!
//! Add a retained yesno version as the fourth argument for a strict pinned read.

use std::error::Error;

use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, QueryParser};
use tantivy::schema::Value;
use tantivy::TantivyDocument;
use yesno_flight::{SetExpr, YesnoClient};
use yesno_tantivy::flight::FlightQueryPreparer;
use yesno_tantivy::FastFieldOrdinalResolver;

#[path = "support/catalog.rs"]
mod catalog;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args().skip(1);
    let endpoint = arguments.next().ok_or_else(|| {
        std::io::Error::other(
            "usage: remote_filter <endpoint> <inventory-key> <delivery-key> [yesno-version]",
        )
    })?;
    let inventory_key = parse_u64(arguments.next(), "inventory-key")?;
    let delivery_key = parse_u64(arguments.next(), "delivery-key")?;
    let pinned_version = arguments
        .next()
        .map(|value| value.parse::<u64>())
        .transpose()?;
    if arguments.next().is_some() {
        return Err(std::io::Error::other("too many arguments").into());
    }

    let catalog = catalog::build()?;
    let reader = catalog.index.reader()?;
    let searcher = reader.searcher();
    let resolver = FastFieldOrdinalResolver::build(&searcher, "product_id")?;

    let mut client = YesnoClient::connect(endpoint).await?;
    let expression = SetExpr::And(vec![
        SetExpr::Key(inventory_key),
        SetExpr::Key(delivery_key),
    ]);
    let mut preparation = FlightQueryPreparer::new(expression).max_matches(100);
    if let Some(version) = pinned_version {
        preparation = preparation.pinned(version);
    }
    let yesno_filter = preparation
        .prepare(&mut client, &searcher, &resolver)
        .await?;
    let yesno_version = yesno_filter
        .yesno_version()
        .expect("remote preparation always records its version");

    let text_query =
        QueryParser::for_index(&catalog.index, vec![catalog.text]).parse_query("cooking")?;
    let query = BooleanQuery::new(vec![
        (Occur::Must, Box::new(yesno_filter) as Box<dyn Query>),
        (Occur::Must, text_query),
    ]);

    let mut products = Vec::new();
    for (_score, address) in searcher.search(&query, &TopDocs::with_limit(10).order_by_score())? {
        let document: TantivyDocument = searcher.doc(address)?;
        let id = document
            .get_first(catalog.product_id)
            .and_then(|value| value.as_u64())
            .ok_or_else(|| std::io::Error::other("stored document has no product_id"))?;
        let text = document
            .get_first(catalog.text)
            .and_then(|value| value.as_str())
            .ok_or_else(|| std::io::Error::other("stored document has no text"))?;
        products.push((id, text.to_owned()));
    }
    products.sort_unstable();

    println!(
        "yesno version {yesno_version}, search `cooking`, in stock, and deliverable => {products:?}"
    );
    Ok(())
}

fn parse_u64(value: Option<String>, name: &str) -> Result<u64, Box<dyn Error>> {
    value
        .ok_or_else(|| std::io::Error::other(format!("missing {name}")))?
        .parse::<u64>()
        .map_err(Into::into)
}
