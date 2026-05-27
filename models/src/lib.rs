#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod agent {
    include!(concat!(env!("OUT_DIR"), "/agent/mod.rs"));
}

#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod events {
    include!(concat!(env!("OUT_DIR"), "/events/mod.rs"));
}
