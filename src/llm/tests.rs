use crate::llm::{LlmClient, LlmContent, OpenAiClientBuilder};
use anyhow::Result;
use futures::stream::StreamExt;
use std::env;

#[tokio::test]
async fn test_llm_content() {
    let delta = LlmContent::Delta("partial response".to_string());
    let final_content = LlmContent::Final("complete response".to_string());

    assert_eq!(delta, LlmContent::Delta("partial response".to_string()));
    assert_eq!(
        final_content,
        LlmContent::Final("complete response".to_string())
    );
    assert_ne!(delta, final_content);
}

#[tokio::test]
#[ignore] // Requires OpenAI API key, run with `cargo test -- --ignored`
async fn test_generate_stream() -> Result<()> {
    if env::var("OPENAI_API_KEY").is_err() {
        println!("Skipping test as OPENAI_API_KEY is not set");
        return Ok(());
    }

    let client = OpenAiClientBuilder::from_env().build()?;
    let mut stream = client.generate("Hello, how are you?").await?;
    let mut content_found = false;
    while let Some(item) = stream.next().await {
        match item? {
            LlmContent::Delta(content) => {
                println!("Delta: {}", content);
                content_found = true;
            }
            LlmContent::Final(content) => {
                println!("Final: {}", content);
                content_found = true;
            }
        }
    }
    assert!(content_found, "No content was received from the LLM");
    Ok(())
}
