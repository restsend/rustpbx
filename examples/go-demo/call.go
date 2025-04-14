package main

import (
	"context"
	"encoding/json"
	"fmt"
	"log"
	"net/url"
	"os"
	"os/signal"
	"sync"

	"github.com/google/uuid"
	"github.com/gorilla/websocket"
	"github.com/sashabaranov/go-openai"
)

// Call represents a call to the rustpbx server
type Call struct {
	client       *Client
	conn         *websocket.Conn
	sessionID    string
	callID       string
	openAIClient *openai.Client
	mu           sync.Mutex
	isConnected  bool
	stopChan     chan struct{}
}

// CallConfig represents the configuration for a call
type CallConfig struct {
	ServerURL    string
	OpenAIAPIKey string
}

// NewCall creates a new call
func NewCall(client *Client, config CallConfig) (*Call, error) {
	// Create a new OpenAI client
	openAIClient := openai.NewClient(config.OpenAIAPIKey)

	return &Call{
		client:       client,
		sessionID:    uuid.New().String(),
		callID:       uuid.New().String(),
		openAIClient: openAIClient,
		stopChan:     make(chan struct{}),
	}, nil
}

// Connect connects to the rustpbx server
func (c *Call) Connect(serverURL string) error {
	c.mu.Lock()
	defer c.mu.Unlock()

	if c.isConnected {
		return fmt.Errorf("already connected")
	}

	// Parse the server URL
	u, err := url.Parse(serverURL)
	if err != nil {
		return fmt.Errorf("failed to parse server URL: %w", err)
	}

	// Add query parameters
	q := u.Query()
	q.Set("id", c.sessionID)
	u.RawQuery = q.Encode()

	// Connect to the WebSocket server
	conn, _, err := websocket.DefaultDialer.Dial(u.String(), nil)
	if err != nil {
		return fmt.Errorf("failed to connect to WebSocket server: %w", err)
	}

	c.conn = conn
	c.isConnected = true

	// Start a goroutine to handle incoming messages
	go c.handleMessages()

	return nil
}

// Disconnect disconnects from the rustpbx server
func (c *Call) Disconnect() error {
	c.mu.Lock()
	defer c.mu.Unlock()

	if !c.isConnected {
		return fmt.Errorf("not connected")
	}

	if c.conn != nil {
		err := c.conn.Close()
		if err != nil {
			return fmt.Errorf("failed to close WebSocket connection: %w", err)
		}
		c.conn = nil
	}

	c.isConnected = false
	return nil
}

// handleMessages handles incoming messages from the rustpbx server
func (c *Call) handleMessages() {
	defer func() {
		c.Disconnect()
	}()

	for {
		select {
		case <-c.stopChan:
			return
		default:
			// Read a message from the WebSocket
			_, message, err := c.conn.ReadMessage()
			if err != nil {
				log.Printf("Error reading message: %v", err)
				return
			}

			// Parse the message as a JSON event
			var event map[string]interface{}
			err = json.Unmarshal(message, &event)
			if err != nil {
				log.Printf("Error parsing event: %v", err)
				continue
			}

			// Handle the event
			c.client.HandleEvent(event)
		}
	}
}

// StartCall starts a call
func (c *Call) StartCall() error {
	c.mu.Lock()
	defer c.mu.Unlock()

	if !c.isConnected {
		return fmt.Errorf("not connected")
	}

	// Create a WebRTC offer
	offer, err := c.client.CreateOffer()
	if err != nil {
		return fmt.Errorf("failed to create offer: %w", err)
	}

	// Create the invite command
	inviteCmd := map[string]interface{}{
		"command": "invite",
		"options": map[string]interface{}{
			"offer": offer.SDP,
		},
	}

	// Send the invite command
	err = c.sendCommand(inviteCmd)
	if err != nil {
		return fmt.Errorf("failed to send invite command: %w", err)
	}

	// Start audio capture
	err = c.client.StartAudioCapture()
	if err != nil {
		return fmt.Errorf("failed to start audio capture: %w", err)
	}

	return nil
}

// EndCall ends a call
func (c *Call) EndCall() error {
	c.mu.Lock()
	defer c.mu.Unlock()

	if !c.isConnected {
		return fmt.Errorf("not connected")
	}

	// Stop audio capture
	err := c.client.StopAudioCapture()
	if err != nil {
		log.Printf("Error stopping audio capture: %v", err)
	}

	// Create the hangup command
	hangupCmd := map[string]interface{}{
		"command": "hangup",
	}

	// Send the hangup command
	err = c.sendCommand(hangupCmd)
	if err != nil {
		return fmt.Errorf("failed to send hangup command: %w", err)
	}

	return nil
}

// SendTTS sends a text-to-speech command
func (c *Call) SendTTS(text string) error {
	c.mu.Lock()
	defer c.mu.Unlock()

	if !c.isConnected {
		return fmt.Errorf("not connected")
	}

	// Create the TTS command
	ttsCmd := map[string]interface{}{
		"command": "tts",
		"text":    text,
	}

	// Send the TTS command
	err := c.sendCommand(ttsCmd)
	if err != nil {
		return fmt.Errorf("failed to send TTS command: %w", err)
	}

	return nil
}

// GenerateLLMResponse generates a response using OpenAI
func (c *Call) GenerateLLMResponse(prompt string) (string, error) {
	// Create a chat completion request
	resp, err := c.openAIClient.CreateChatCompletion(
		context.Background(),
		openai.ChatCompletionRequest{
			Model: openai.GPT4,
			Messages: []openai.ChatCompletionMessage{
				{
					Role:    openai.ChatMessageRoleUser,
					Content: prompt,
				},
			},
		},
	)
	if err != nil {
		return "", fmt.Errorf("failed to generate LLM response: %w", err)
	}

	// Extract the response text
	if len(resp.Choices) == 0 {
		return "", fmt.Errorf("no response from LLM")
	}

	return resp.Choices[0].Message.Content, nil
}

// sendCommand sends a command to the rustpbx server
func (c *Call) sendCommand(cmd map[string]interface{}) error {
	// Marshal the command to JSON
	data, err := json.Marshal(cmd)
	if err != nil {
		return fmt.Errorf("failed to marshal command: %w", err)
	}

	// Send the command
	err = c.conn.WriteMessage(websocket.TextMessage, data)
	if err != nil {
		return fmt.Errorf("failed to send command: %w", err)
	}

	return nil
}

// WaitForInterrupt waits for an interrupt signal
func (c *Call) WaitForInterrupt() {
	// Create a channel to receive interrupt signals
	interrupt := make(chan os.Signal, 1)
	signal.Notify(interrupt, os.Interrupt)

	// Wait for an interrupt signal
	<-interrupt

	// End the call
	err := c.EndCall()
	if err != nil {
		log.Printf("Error ending call: %v", err)
	}

	// Disconnect from the server
	err = c.Disconnect()
	if err != nil {
		log.Printf("Error disconnecting: %v", err)
	}
}
