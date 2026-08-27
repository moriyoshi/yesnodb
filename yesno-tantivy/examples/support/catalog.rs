//! The three-product e-commerce fixture from Tantivy's `warmer` example.

use tantivy::schema::{Field, Schema, FAST, STORED, TEXT};
use tantivy::{doc, Index, IndexWriter};

pub type ProductId = u64;

pub const OLIVE_OIL: ProductId = 323_423;
pub const GLOVES: ProductId = 3_966_623;
pub const SNEAKERS: ProductId = 23_222;

pub struct ProductCatalog {
    pub index: Index,
    pub product_id: Field,
    pub text: Field,
}

pub fn build() -> tantivy::Result<ProductCatalog> {
    let mut schema = Schema::builder();
    // Tantivy's fixture needs `FAST` for its external-price warmer. yesno uses
    // the same field to translate stable product IDs into segment-local DocIds.
    // `STORED` is added so these examples can print useful results.
    let product_id = schema.add_u64_field("product_id", FAST | STORED);
    let text = schema.add_text_field("text", TEXT | STORED);
    let index = Index::create_in_ram(schema.build());

    let mut writer: IndexWriter = index.writer_with_num_threads(1, 15_000_000)?;
    writer.add_document(doc!(product_id => OLIVE_OIL, text => "cooking olive oil from greece"))?;
    writer
        .add_document(doc!(product_id => GLOVES, text => "kitchen gloves, perfect for cooking"))?;
    writer.add_document(doc!(product_id => SNEAKERS, text => "uber sweet sneakers"))?;
    writer.commit()?;

    Ok(ProductCatalog {
        index,
        product_id,
        text,
    })
}
