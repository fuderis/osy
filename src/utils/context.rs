use crate::prelude::*;

use anylm::embeddings::{Embeddings, Search};

/// Generates the text embeddings.
pub async fn generate_embedding(text: &str, search: Search) -> Result<Vec<f32>> {
    let embeddings = Embeddings::try_from(Config::get().llm.embeddings_options())?
        .input(text)
        .search(search)
        .send()
        .await?;

    let first = embeddings
        .data
        .into_iter()
        .next()
        .ok_or(Error::NoEmbeddingReceived)?;

    Ok(first.embedding)
}
