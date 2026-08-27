//! Filter Tantivy's e-commerce warmer fixture with live yesno product cohorts.
//!
//! Run with:
//!
//! ```text
//! cargo run -p yesno-tantivy --example embedded_filter
//! ```

use std::error::Error;

use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, QueryParser};
use tantivy::schema::Value;
use tantivy::TantivyDocument;
use yesno_core::{Db, Expr};
use yesno_tantivy::{FastFieldOrdinalResolver, MissingOrdinalPolicy, PreparedYesnoQuery};

#[path = "support/catalog.rs"]
mod catalog;

const IN_STOCK: u64 = 1;
const DELIVERS_TO_CUSTOMER: u64 = 2;

fn main() -> Result<(), Box<dyn Error>> {
    let catalog = catalog::build()?;
    let reader = catalog.index.reader()?;
    let searcher = reader.searcher();
    let resolver = FastFieldOrdinalResolver::build(&searcher, "product_id")?;

    // Operational product state changes more often than the searchable catalog,
    // so it lives in yesno. For this customer, only olive oil is both in stock
    // and deliverable; gloves are in stock but cannot be delivered, while the
    // sneakers are deliverable but out of stock.
    let directory = tempfile::tempdir()?;
    let db = Db::open(directory.path())?;
    db.insert_many(IN_STOCK, &[catalog::OLIVE_OIL, catalog::GLOVES])?;
    db.insert_many(
        DELIVERS_TO_CUSTOMER,
        &[catalog::OLIVE_OIL, catalog::SNEAKERS],
    )?;
    let snapshot = db.snapshot()?;

    let yesno_expression =
        Expr::set(snapshot.load(IN_STOCK)?).and(Expr::set(snapshot.load(DELIVERS_TO_CUSTOMER)?));
    let yesno_filter = PreparedYesnoQuery::from_expr(
        &searcher,
        &resolver,
        &yesno_expression,
        MissingOrdinalPolicy::Error,
        Some(snapshot.version()),
    )?;

    // This is the same full-text query used by Tantivy's warmer example. Its
    // two text matches are reduced to the one product this customer can buy.
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

    assert_eq!(
        products,
        vec![(
            catalog::OLIVE_OIL,
            "cooking olive oil from greece".to_owned()
        )]
    );
    println!("search `cooking`, in stock, and deliverable => {products:?}");
    Ok(())
}
