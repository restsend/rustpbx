use anyhow::Result;
use async_openai::{
    config::OpenAIConfig,
    types::{
        ChatCompletionRequestAssistantMessageArgs, ChatCompletionRequestMessage,
        ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestUserMessageArgs,
        CreateChatCompletionRequestArgs, Role,
    },
    Client,
};
use dotenv::dotenv;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::{collections::VecDeque, sync::Arc, time::SystemTime};
use tokio::sync::broadcast;

use crate::event::{EventSender, SessionEvent};

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
        Self {
            model: "gpt-3.5-turbo".to_string(),
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

// LLM client trait - to be implemented with actual LLM integration
pub trait LlmClient: Send + Sync {
    fn generate_response<'a>(
        &'a self,
        input: &'a str,
        config: &'a LlmConfig,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>>;

    fn generate_stream<'a>(
        &'a self,
        input: &'a str,
        config: &'a LlmConfig,
        event_sender: EventSender,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>>;
}

// Builder for OpenAI Client
pub struct OpenAiClientBuilder {
    api_key: Option<String>,
    base_url: Option<String>,
    org_id: Option<String>,
    max_conversation_turns: usize,
}

impl Default for OpenAiClientBuilder {
    fn default() -> Self {
        Self {
            api_key: None,
            base_url: None,
            org_id: None,
            max_conversation_turns: 10,
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

        Self {
            api_key,
            base_url,
            org_id,
            max_conversation_turns: max_turns,
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
        })
    }
}

// OpenAI LLM client implementation
pub struct OpenAiClient {
    client: Client<OpenAIConfig>,
    conversation: VecDeque<Message>,
    max_conversation_turns: usize,
}

impl OpenAiClient {
    pub fn new(api_key: impl Into<String>) -> Result<Self> {
        OpenAiClientBuilder::new().with_api_key(api_key).build()
    }

    pub fn from_env() -> Result<Self> {
        OpenAiClientBuilder::from_env().build()
    }

    fn prepare_messages(
        &mut self,
        system_prompt: &str,
        input: &str,
    ) -> Result<Vec<ChatCompletionRequestMessage>> {
        // Add the new user message to the conversation
        self.conversation.push_back(Message::user(input));

        // Ensure we don't exceed max_conversation_turns
        while self.conversation.len() > self.max_conversation_turns {
            self.conversation.pop_front();
        }

        // Convert to OpenAI format
        let mut messages = Vec::new();

        // Add system prompt
        messages.push(Message::system(system_prompt).to_openai_message()?);

        // Add conversation history
        for message in &self.conversation {
            messages.push(message.to_openai_message()?);
        }

        Ok(messages)
    }

    pub fn add_assistant_response(&mut self, response: &str) {
        self.conversation.push_back(Message::assistant(response));

        // Ensure we don't exceed max_conversation_turns
        while self.conversation.len() > self.max_conversation_turns {
            self.conversation.pop_front();
        }
    }
}

impl LlmClient for OpenAiClient {
    fn generate_response<'a>(
        &'a self,
        input: &'a str,
        config: &'a LlmConfig,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>> {
        // Clone necessary values for use in the async block
        let client_instance = self.client.clone();
        let mut conversation = self.conversation.clone();
        let max_turns = self.max_conversation_turns;
        let model = config.model.clone();
        let system_prompt = config.prompt.clone();
        let temperature = config.temperature.unwrap_or(0.7);
        let max_tokens = config.max_tokens;
        let input = input.to_string();

        Box::pin(async move {
            // Add the new user message to the conversation
            conversation.push_back(Message::user(&input));

            // Ensure we don't exceed max_conversation_turns
            while conversation.len() > max_turns {
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
                .model(&model)
                .messages(messages)
                .temperature(temperature)
                .build()?;

            // Send the request and get the response
            let response = client_instance.chat().create(request).await?;
            let response_text = response.choices[0]
                .message
                .content
                .clone()
                .unwrap_or_default();

            // Add the assistant response to the conversation
            conversation.push_back(Message::assistant(&response_text));

            Ok(response_text)
        })
    }

    fn generate_stream<'a>(
        &'a self,
        input: &'a str,
        config: &'a LlmConfig,
        event_sender: EventSender,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>> {
        // Clone necessary values for use in the async block
        let client_instance = self.client.clone();
        let mut conversation = self.conversation.clone();
        let max_turns = self.max_conversation_turns;
        let model = config.model.clone();
        let system_prompt = config.prompt.clone();
        let temperature = config.temperature.unwrap_or(0.7);
        let max_tokens = config.max_tokens;
        let input = input.to_string();

        Box::pin(async move {
            // Add the new user message to the conversation
            conversation.push_back(Message::user(&input));

            // Ensure we don't exceed max_conversation_turns
            while conversation.len() > max_turns {
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
                .model(&model)
                .messages(messages)
                .temperature(temperature)
                .stream(true)
                .build()?;

            // Send the request and get the streaming response
            let mut stream = client_instance.chat().create_stream(request).await?;

            let mut full_text = String::new();
            let timestamp = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as u32;

            // Process the stream
            while let Some(result) = stream.next().await {
                match result {
                    Ok(response) => {
                        if let Some(content) = response
                            .choices
                            .get(0)
                            .and_then(|c| c.delta.content.as_ref())
                        {
                            full_text.push_str(content);

                            // Send the event
                            let _ =
                                event_sender.send(SessionEvent::LLM(timestamp, full_text.clone()));
                        }
                    }
                    Err(err) => {
                        return Err(anyhow::anyhow!("Stream error: {}", err));
                    }
                }
            }

            // Add the assistant response to the conversation
            conversation.push_back(Message::assistant(&full_text));

            Ok(full_text)
        })
    }
}

// LLM Processor to integrate with media stream processing
pub struct LlmProcessor {
    config: LlmConfig,
    client: Arc<dyn LlmClient>,
    event_sender: EventSender,
}

impl LlmProcessor {
    pub fn new(config: LlmConfig, client: Arc<dyn LlmClient>, event_sender: EventSender) -> Self {
        Self {
            config,
            client,
            event_sender,
        }
    }

    // Process text input and generate a response
    pub async fn process(&self, input: &str) -> Result<String> {
        // Generate response using the LLM client
        let timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32;

        // Send initial processing event
        let _ = self
            .event_sender
            .send(SessionEvent::LLM(timestamp, "Processing...".to_string()));

        let response = if self.config.stream.unwrap_or(true) {
            // Generate streaming response
            self.client
                .generate_stream(input, &self.config, self.event_sender.clone())
                .await?
        } else {
            // Generate non-streaming response
            let response = self.client.generate_response(input, &self.config).await?;

            // Send final response event
            let _ = self
                .event_sender
                .send(SessionEvent::LLM(timestamp, response.clone()));

            response
        };

        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Mock LLM client for testing
    struct MockLlmClient {
        responses: Mutex<VecDeque<String>>,
    }

    impl MockLlmClient {
        fn new(responses: Vec<String>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
            }
        }
    }

    impl LlmClient for MockLlmClient {
        fn generate_response<'a>(
            &'a self,
            _input: &'a str,
            _config: &'a LlmConfig,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>>
        {
            let responses = self.responses.lock().unwrap();
            let response = responses
                .front()
                .cloned()
                .unwrap_or_else(|| "Default response".to_string());

            Box::pin(async move { Ok(response) })
        }

        fn generate_stream<'a>(
            &'a self,
            _input: &'a str,
            _config: &'a LlmConfig,
            event_sender: EventSender,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>>
        {
            let response = {
                let responses = self.responses.lock().unwrap();
                responses
                    .front()
                    .cloned()
                    .unwrap_or_else(|| "Default response".to_string())
            };

            let timestamp = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as u32;

            Box::pin(async move {
                // Simulate streaming by sending parts of the response
                let chars: Vec<char> = response.chars().collect();
                let mut current = String::new();

                for chunk in chars.chunks(5) {
                    current.extend(chunk.iter());
                    let _ = event_sender.send(SessionEvent::LLM(timestamp, current.clone()));
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                }

                Ok(response)
            })
        }
    }

    #[tokio::test]
    async fn test_mock_llm_client() {
        let client = MockLlmClient::new(vec!["This is a test response".to_string()]);
        let config = LlmConfig::default();

        let response = client.generate_response("Hello", &config).await;
        assert!(response.is_ok());
        assert_eq!(response.unwrap(), "This is a test response");
    }

    #[tokio::test]
    async fn test_llm_processor() {
        let client = Arc::new(MockLlmClient::new(vec![
            "This is a test response".to_string()
        ]));
        let config = LlmConfig::default();

        let (event_sender, mut event_receiver) = broadcast::channel(10);
        let processor = LlmProcessor::new(config, client, event_sender);

        // Process in a separate task
        let process_task = tokio::spawn(async move { processor.process("Hello").await });

        // Collect events
        let mut events = Vec::new();
        while let Ok(event) = event_receiver.recv().await {
            if let SessionEvent::LLM(_, text) = event {
                events.push(text.clone());
                if text == "This is a test response" {
                    break;
                }
            }
        }

        // Verify process result
        let result = process_task.await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "This is a test response");

        // Verify we got some events
        assert!(!events.is_empty());
        assert_eq!(events.last().unwrap(), "This is a test response");
    }
}
