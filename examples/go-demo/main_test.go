package main

import (
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/joho/godotenv"
)

func init() {
	// Load .env file from the current directory first
	if err := godotenv.Load(); err != nil {
		// Try loading from the project root if not found in current directory
		envPath := filepath.Join("..", "..", ".env")
		if err := godotenv.Load(envPath); err != nil {
			// It's okay if .env doesn't exist, we'll use environment variables
		}
	}
}

func TestDeviceManager(t *testing.T) {
	// Skip if running in CI environment
	if os.Getenv("CI") != "" {
		t.Skip("Skipping test in CI environment")
	}

	// Create a device manager
	deviceManager, err := NewDeviceManager()
	if err != nil {
		t.Fatalf("Failed to create device manager: %v", err)
	}
	defer deviceManager.Close()

	// Get all devices
	devices := deviceManager.GetDevices()
	if len(devices) == 0 {
		t.Log("No audio devices found, this might be normal in some environments")
	}

	// Get input devices
	inputDevices := deviceManager.GetInputDevices()
	if len(inputDevices) == 0 {
		t.Log("No input devices found, this might be normal in some environments")
	}

	// Get default input device
	defaultInputDevice := deviceManager.GetDefaultInputDevice()
	if defaultInputDevice == nil {
		t.Log("No default input device found, this might be normal in some environments")
	}
}

func TestClient(t *testing.T) {
	// Skip if running in CI environment
	if os.Getenv("CI") != "" {
		t.Skip("Skipping test in CI environment")
	}

	// Create a device manager
	deviceManager, err := NewDeviceManager()
	if err != nil {
		t.Fatalf("Failed to create device manager: %v", err)
	}
	defer deviceManager.Close()

	// Create a client
	client, err := NewClient(deviceManager)
	if err != nil {
		t.Fatalf("Failed to create client: %v", err)
	}
	defer client.Close()

	// Create an offer
	offer, err := client.CreateOffer()
	if err != nil {
		t.Fatalf("Failed to create offer: %v", err)
	}

	if offer.SDP == "" {
		t.Fatal("Offer SDP is empty")
	}
}

func TestCall(t *testing.T) {
	// Skip if running in CI environment
	if os.Getenv("CI") != "" {
		t.Skip("Skipping test in CI environment")
	}

	// Get OpenAI API key from environment
	openAIAPIKey := os.Getenv("OPENAI_API_KEY")
	if openAIAPIKey == "" {
		t.Skip("Skipping test: OpenAI API key not provided")
	}

	// Get server URL from environment or use default
	serverURL := os.Getenv("SERVER_URL")
	if serverURL == "" {
		serverURL = "ws://localhost:8080/webrtc"
	}

	// Create a device manager
	deviceManager, err := NewDeviceManager()
	if err != nil {
		t.Fatalf("Failed to create device manager: %v", err)
	}
	defer deviceManager.Close()

	// Create a client
	client, err := NewClient(deviceManager)
	if err != nil {
		t.Fatalf("Failed to create client: %v", err)
	}
	defer client.Close()

	// Create a call
	call, err := NewCall(client, CallConfig{
		ServerURL:    serverURL,
		OpenAIAPIKey: openAIAPIKey,
	})
	if err != nil {
		t.Fatalf("Failed to create call: %v", err)
	}

	// Test LLM response generation
	prompt := "Say hello in Chinese."
	response, err := call.GenerateLLMResponse(prompt)
	if err != nil {
		t.Fatalf("Failed to generate LLM response: %v", err)
	}

	if response == "" {
		t.Fatal("LLM response is empty")
	}
}

func TestIntegration(t *testing.T) {
	// Skip if running in CI environment
	if os.Getenv("CI") != "" {
		t.Skip("Skipping test in CI environment")
	}

	// Get OpenAI API key from environment
	openAIAPIKey := os.Getenv("OPENAI_API_KEY")
	if openAIAPIKey == "" {
		t.Skip("Skipping test: OpenAI API key not provided")
	}

	// Get server URL from environment or use default
	serverURL := os.Getenv("SERVER_URL")
	if serverURL == "" {
		serverURL = "ws://localhost:8080/webrtc"
	}

	// Create a device manager
	deviceManager, err := NewDeviceManager()
	if err != nil {
		t.Fatalf("Failed to create device manager: %v", err)
	}
	defer deviceManager.Close()

	// Create a client
	client, err := NewClient(deviceManager)
	if err != nil {
		t.Fatalf("Failed to create client: %v", err)
	}
	defer client.Close()

	// Create a call
	call, err := NewCall(client, CallConfig{
		ServerURL:    serverURL,
		OpenAIAPIKey: openAIAPIKey,
	})
	if err != nil {
		t.Fatalf("Failed to create call: %v", err)
	}

	// Connect to the server
	err = call.Connect(serverURL)
	if err != nil {
		t.Fatalf("Failed to connect to server: %v", err)
	}
	defer call.Disconnect()

	// Start a call
	err = call.StartCall()
	if err != nil {
		t.Fatalf("Failed to start call: %v", err)
	}
	defer call.EndCall()

	// Wait for a short time to establish the connection
	time.Sleep(2 * time.Second)

	// Send a TTS command
	err = call.SendTTS("Hello, this is a test call from the Go demo.")
	if err != nil {
		t.Fatalf("Failed to send TTS command: %v", err)
	}

	// Wait for a short time to hear the TTS
	time.Sleep(3 * time.Second)

	// End the call
	err = call.EndCall()
	if err != nil {
		t.Fatalf("Failed to end call: %v", err)
	}
}
