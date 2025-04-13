use anyhow::Result;
use async_openai::{
    config::OpenAIConfig,
    types::{
        ChatCompletionRequestAssistantMessageArgs, ChatCompletionRequestMessage,
        ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestUserMessageArgs,
        CreateChatCompletionRequestArgs,
    },
    Client,
};
use async_trait::async_trait;
use dotenv::dotenv;
use futures::stream;
use futures::stream::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Arc;
#[cfg(test)]
mod tests;
// Configuration for Language Model
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LlmConfig {
    pub model: String,
    pub prompt: String,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub max_conversation_turns: Option<usize>,
    pub stream: Option<bool>,
    pub base_url: Option<String>,
    pub tools: Option<Vec<serde_json::Value>>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        // Try to load environment variables if not already done
        let _ = dotenv();

        // Get model from environment variable, or use default
        let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-3.5-turbo".to_string());

        Self {
            model,
            prompt: "You are a helpful assistant.".to_string(),
            temperature: Some(0.7),
            max_tokens: Some(1000),
            max_conversation_turns: Some(10),
            stream: Some(true),
            base_url: None,
            tools: None,
        }
    }
}

// Conversation message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: content.into(),
        }
    }

    pub fn to_openai_message(&self) -> Result<ChatCompletionRequestMessage> {
        match self.role.as_str() {
            "user" => Ok(ChatCompletionRequestUserMessageArgs::default()
                .content(&*self.content)
                .build()?
                .into()),
            "assistant" => Ok(ChatCompletionRequestAssistantMessageArgs::default()
                .content(&*self.content)
                .build()?
                .into()),
            "system" => Ok(ChatCompletionRequestSystemMessageArgs::default()
                .content(&*self.content)
                .build()?
                .into()),
            _ => Err(anyhow::anyhow!("Invalid role: {}", self.role)),
        }
    }
}

// Content variants from LLM
#[derive(Debug, Clone, PartialEq)]
pub enum LlmContent {
    Delta(String),
    Final(String),
}

// Simplified LLM client trait
#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn generate(
        &self,
        prompt: &str,
    ) -> Result<Box<dyn Stream<Item = Result<LlmContent>> + Send + Unpin + '_>>;
}

// Builder for OpenAI Client
pub struct OpenAiClientBuilder {
    api_key: Option<String>,
    base_url: Option<String>,
    org_id: Option<String>,
    max_conversation_turns: usize,
    model: Option<String>,
}

impl Default for OpenAiClientBuilder {
    fn default() -> Self {
        Self {
            api_key: None,
            base_url: None,
            org_id: None,
            max_conversation_turns: 10,
            model: None,
        }
    }
}

impl OpenAiClientBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

    pub fn with_org_id(mut self, org_id: impl Into<String>) -> Self {
        self.org_id = Some(org_id.into());
        self
    }

    pub fn with_max_conversation_turns(mut self, max_turns: usize) -> Self {
        self.max_conversation_turns = max_turns;
        self
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn from_env() -> Self {
        // Load .env file if it exists
        let _ = dotenv();

        let api_key = std::env::var("OPENAI_API_KEY").ok();
        let base_url = std::env::var("OPENAI_BASE_URL").ok();
        let org_id = std::env::var("OPENAI_ORG_ID").ok();
        let max_turns = std::env::var("OPENAI_MAX_TURNS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(10);
        let model = std::env::var("OPENAI_MODEL").ok();

        Self {
            api_key,
            base_url,
            org_id,
            max_conversation_turns: max_turns,
            model,
        }
    }

    pub fn build(self) -> Result<OpenAiClient> {
        let api_key = self
            .api_key
            .ok_or_else(|| anyhow::anyhow!("API key is required"))?;

        let mut config = OpenAIConfig::new().with_api_key(api_key);

        if let Some(base_url) = self.base_url {
            config = config.with_api_base(base_url);
        }

        if let Some(org_id) = self.org_id {
            config = config.with_org_id(org_id);
        }

        let client = Client::with_config(config);

        Ok(OpenAiClient {
            client,
            conversation: VecDeque::new(),
            max_conversation_turns: self.max_conversation_turns,
            config: LlmConfig::default(),
        })
    }
}

#[derive(Debug)]
pub struct OpenAiClient {
    client: Client<OpenAIConfig>,
    conversation: VecDeque<Message>,
    max_conversation_turns: usize,
    config: LlmConfig,
}

impl OpenAiClient {
    pub fn new(api_key: impl Into<String>) -> Result<Self> {
        OpenAiClientBuilder::new().with_api_key(api_key).build()
    }

    pub fn from_env() -> Result<Self> {
        OpenAiClientBuilder::from_env().build()
    }

    pub fn with_config(mut self, config: LlmConfig) -> Self {
        self.config = config;
        self
    }

    pub fn add_assistant_response(&mut self, response: &str) {
        self.conversation.push_back(Message::assistant(response));

        // Ensure we don't exceed max_conversation_turns
        while self.conversation.len() > self.max_conversation_turns {
            self.conversation.pop_front();
        }
    }

    pub async fn generate_completion(
        &self,
        input: &str,
    ) -> Result<Box<dyn Stream<Item = Result<LlmContent>> + Send + Unpin + '_>> {
        let system_prompt = self.config.prompt.clone();
        let temperature = self.config.temperature.unwrap_or(0.7);
        let max_tokens = self.config.max_tokens;
        let stream = self.config.stream.unwrap_or(true);

        // Clone conversation for use in async block
        let mut conversation = self.conversation.clone();

        // Add the new user message to the conversation
        conversation.push_back(Message::user(input));

        // Ensure we don't exceed max_conversation_turns
        while conversation.len() > self.max_conversation_turns {
            conversation.pop_front();
        }

        // Convert to OpenAI format
        let mut messages = Vec::new();

        // Add system prompt
        messages.push(
            ChatCompletionRequestSystemMessageArgs::default()
                .content(&*system_prompt)
                .build()?
                .into(),
        );

        // Add conversation history
        for message in &conversation {
            messages.push(message.to_openai_message()?);
        }

        // Create the request
        let request = CreateChatCompletionRequestArgs::default()
            .max_tokens(if let Some(max_tokens) = max_tokens {
                max_tokens as u32
            } else {
                512u32
            })
            .model(&self.config.model)
            .messages(messages)
            .temperature(temperature)
            .stream(stream)
            .build()?;

        // Record request start time for TTFB measurement
        let _request_start_time = std::time::Instant::now();

        let client = self.client.clone();

        if stream {
            // Send the streaming request and return as a stream
            let stream = client.chat().create_stream(request).await?;
            let full_content = Arc::new(std::sync::Mutex::new(String::new()));
            let full_content_clone = full_content.clone();
            let content_stream = stream
                .map(move |result| {
                    result
                        .map_err(|e| anyhow::anyhow!("Stream error: {}", e))
                        .and_then(|response| {
                            let content = response
                                .choices
                                .get(0)
                                .and_then(|c| c.delta.content.as_ref())
                                .map(|s| s.clone())
                                .unwrap_or_default();

                            if !content.is_empty() {
                                full_content_clone.lock().unwrap().push_str(&content);
                                Ok(LlmContent::Delta(content))
                            } else {
                                // Skip empty content
                                Err(anyhow::anyhow!("Empty content"))
                            }
                        })
                })
                .filter_map(|result| async move {
                    match result {
                        Ok(content) => Some(Ok(content)),
                        Err(e) => {
                            if e.to_string() == "Empty content" {
                                None // Filter out empty content errors
                            } else {
                                Some(Err(e))
                            }
                        }
                    }
                });

            // Add the final message at the end
            let final_stream = content_stream.chain(stream::once(async move {
                Ok(LlmContent::Final(full_content.lock().unwrap().clone()))
            }));

            Ok(Box::new(Box::pin(final_stream)))
        } else {
            // Non-streaming mode - just return the final result
            let response = client.chat().create(request).await?;
            let content = response.choices[0]
                .message
                .content
                .clone()
                .unwrap_or_default();

            // Return a stream with just the final content
            Ok(Box::new(Box::pin(stream::once(async move {
                Ok(LlmContent::Final(content))
            }))))
        }
    }
}

#[async_trait]
impl LlmClient for OpenAiClient {
    async fn generate(
        &self,
        prompt: &str,
    ) -> Result<Box<dyn Stream<Item = Result<LlmContent>> + Send + Unpin + '_>> {
        let response = self.generate_completion(prompt).await?;
        Ok(response)
    }
}
