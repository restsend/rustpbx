package main

import (
	"flag"
	"fmt"
	"log"
	"os"
	"os/signal"
	"syscall"
	"time"
)

func main() {
	// Parse command-line flags
	serverURL := flag.String("server", "ws://rustpbx.com/webrtc", "WebSocket server URL")
	openAIAPIKey := flag.String("openai-key", "", "OpenAI API key")
	flag.Parse()

	// Check if OpenAI API key is provided
	if *openAIAPIKey == "" {
		*openAIAPIKey = os.Getenv("OPENAI_API_KEY")
		if *openAIAPIKey == "" {
			log.Fatal("OpenAI API key is required. Set it with -openai-key or OPENAI_API_KEY environment variable.")
		}
	}

	// Create a device manager
	deviceManager, err := NewDeviceManager()
	if err != nil {
		log.Fatalf("Failed to create device manager: %v", err)
	}
	defer deviceManager.Close()

	// Print available devices
	fmt.Println("Available audio devices:")
	for _, device := range deviceManager.GetDevices() {
		deviceType := "Output"
		if device.IsInput {
			deviceType = "Input"
		}
		fmt.Printf("  %s: %s (ID: %s)\n", deviceType, device.Name, device.ID)
	}

	// Create a client
	client, err := NewClient(deviceManager)
	if err != nil {
		log.Fatalf("Failed to create client: %v", err)
	}
	defer client.Close()

	// Create a call
	call, err := NewCall(client, CallConfig{
		ServerURL:    *serverURL,
		OpenAIAPIKey: *openAIAPIKey,
	})
	if err != nil {
		log.Fatalf("Failed to create call: %v", err)
	}

	// Connect to the server
	err = call.Connect(*serverURL)
	if err != nil {
		log.Fatalf("Failed to connect to server: %v", err)
	}
	defer call.Disconnect()

	// Start a call
	err = call.StartCall()
	if err != nil {
		log.Fatalf("Failed to start call: %v", err)
	}
	defer call.EndCall()

	// Create a channel to receive interrupt signals
	interrupt := make(chan os.Signal, 1)
	signal.Notify(interrupt, os.Interrupt, syscall.SIGTERM)

	// Wait for a short time to establish the connection
	time.Sleep(2 * time.Second)

	// Send a TTS command
	err = call.SendTTS("Hello, this is a test call from the Go demo.")
	if err != nil {
		log.Printf("Failed to send TTS command: %v", err)
	}

	// Generate a response using OpenAI
	prompt := "Generate a short greeting in Chinese."
	response, err := call.GenerateLLMResponse(prompt)
	if err != nil {
		log.Printf("Failed to generate LLM response: %v", err)
	} else {
		fmt.Printf("LLM Response: %s\n", response)

		// Send the response as TTS
		err = call.SendTTS(response)
		if err != nil {
			log.Printf("Failed to send TTS command: %v", err)
		}
	}

	// Wait for an interrupt signal
	<-interrupt

	fmt.Println("Call ended.")
}
