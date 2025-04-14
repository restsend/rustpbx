package main

import (
	"fmt"
	"io"
	"log"
	"sync"
	"time"
	"unsafe"

	"github.com/gen2brain/malgo"
	"github.com/pion/webrtc/v3"
	"github.com/pion/webrtc/v3/pkg/media"
	"github.com/shenjinti/go722"
)

// Client represents a WebRTC client
type Client struct {
	deviceManager  *DeviceManager
	peerConnection *webrtc.PeerConnection
	audioTrack     *webrtc.TrackLocalStaticSample
	g722Encoder    *go722.G722Encoder
	audioDevice    *malgo.Device
	audioBuffer    []byte
	mu             sync.Mutex
	isRunning      bool
	stopChan       chan struct{}
}

// NewClient creates a new client
func NewClient(deviceManager *DeviceManager) (*Client, error) {
	// Create a new WebRTC API with G.722 codec
	mediaEngine := webrtc.MediaEngine{}
	if err := mediaEngine.RegisterCodec(webrtc.RTPCodecParameters{
		RTPCodecCapability: webrtc.RTPCodecCapability{
			MimeType:     webrtc.MimeTypeG722,
			ClockRate:    8000,
			Channels:     1,
			SDPFmtpLine:  "",
			RTCPFeedback: nil,
		},
		PayloadType: 9,
	}, webrtc.RTPCodecTypeAudio); err != nil {
		return nil, fmt.Errorf("failed to register G.722 codec: %w", err)
	}

	api := webrtc.NewAPI(webrtc.WithMediaEngine(&mediaEngine))

	// Create a new peer connection
	peerConnection, err := api.NewPeerConnection(webrtc.Configuration{
		ICEServers: []webrtc.ICEServer{
			{
				URLs: []string{"stun:stun.l.google.com:19302"},
			},
		},
	})
	if err != nil {
		return nil, fmt.Errorf("failed to create peer connection: %w", err)
	}

	// Create a new G.722 encoder
	g722Encoder := go722.NewG722Encoder(go722.Rate48000, 0)
	if err != nil {
		return nil, fmt.Errorf("failed to create G.722 encoder: %w", err)
	}

	// Create a new audio track
	audioTrack, err := webrtc.NewTrackLocalStaticSample(webrtc.RTPCodecCapability{
		MimeType:     webrtc.MimeTypeG722,
		ClockRate:    8000,
		Channels:     1,
		SDPFmtpLine:  "",
		RTCPFeedback: nil,
	}, "audio", "pion")
	if err != nil {
		return nil, fmt.Errorf("failed to create audio track: %w", err)
	}

	// Add the track to the peer connection
	rtpSender, err := peerConnection.AddTrack(audioTrack)
	if err != nil {
		return nil, fmt.Errorf("failed to add track to peer connection: %w", err)
	}

	// Start a goroutine to handle incoming RTCP packets
	go func() {
		rtcpBuf := make([]byte, 1500)
		for {
			_, _, err := rtpSender.Read(rtcpBuf)
			if err != nil {
				if err != io.EOF {
					log.Printf("Error reading RTCP: %v", err)
				}
				return
			}
		}
	}()

	return &Client{
		deviceManager:  deviceManager,
		peerConnection: peerConnection,
		audioTrack:     audioTrack,
		g722Encoder:    g722Encoder,
		audioBuffer:    make([]byte, 160), // 10ms at 16kHz
		stopChan:       make(chan struct{}),
	}, nil
}

// Close closes the client
func (c *Client) Close() {
	c.mu.Lock()
	defer c.mu.Unlock()

	if c.isRunning {
		close(c.stopChan)
		c.isRunning = false
	}

	if c.audioDevice != nil {
		c.audioDevice.Uninit()
	}

	if c.peerConnection != nil {
		c.peerConnection.Close()
	}
}

// CreateOffer creates a WebRTC offer
func (c *Client) CreateOffer() (webrtc.SessionDescription, error) {
	offer, err := c.peerConnection.CreateOffer(nil)
	if err != nil {
		return webrtc.SessionDescription{}, fmt.Errorf("failed to create offer: %w", err)
	}

	err = c.peerConnection.SetLocalDescription(offer)
	if err != nil {
		return webrtc.SessionDescription{}, fmt.Errorf("failed to set local description: %w", err)
	}

	return offer, nil
}

// SetRemoteDescription sets the remote description
func (c *Client) SetRemoteDescription(sdp string) error {
	answer := webrtc.SessionDescription{
		Type: webrtc.SDPTypeAnswer,
		SDP:  sdp,
	}

	err := c.peerConnection.SetRemoteDescription(answer)
	if err != nil {
		return fmt.Errorf("failed to set remote description: %w", err)
	}

	return nil
}

// StartAudioCapture starts capturing audio from the default input device
func (c *Client) StartAudioCapture() error {
	c.mu.Lock()
	defer c.mu.Unlock()

	if c.isRunning {
		return fmt.Errorf("audio capture is already running")
	}

	// Get the default input device
	inputDevice := c.deviceManager.GetDefaultInputDevice()
	if inputDevice == nil {
		return fmt.Errorf("no input device found")
	}

	// Create a new audio device
	deviceConfig := malgo.DefaultDeviceConfig(malgo.Capture)
	deviceConfig.Capture.Format = malgo.FormatS16
	deviceConfig.Capture.Channels = 1
	deviceConfig.SampleRate = 16000
	deviceConfig.Alsa.NoMMap = 1

	// Convert device ID to unsafe.Pointer
	deviceID := inputDevice.ID
	deviceConfig.Capture.DeviceID = unsafe.Pointer(&deviceID)

	// Create a callback function to handle audio data
	callbacks := malgo.DeviceCallbacks{
		Data: func(outputSamples, inputSamples []byte, framecount uint32) {
			// Copy the audio data to our buffer
			copy(c.audioBuffer, inputSamples)

			// Encode the audio data to G.722
			g722Data := make([]byte, len(c.audioBuffer)/2)
			c.g722Encoder.Encode(g722Data)

			// Send the G.722 data to the WebRTC track
			err := c.audioTrack.WriteSample(media.Sample{
				Data:     g722Data,
				Duration: time.Millisecond * 10,
			})
			if err != nil {
				log.Printf("Error writing sample: %v", err)
			}
		},
	}

	// Create a new audio device
	device, err := malgo.InitDevice(c.deviceManager.context.Context, deviceConfig, callbacks)
	if err != nil {
		return fmt.Errorf("failed to initialize audio device: %w", err)
	}

	// Start the audio device
	err = device.Start()
	if err != nil {
		return fmt.Errorf("failed to start audio device: %w", err)
	}

	c.audioDevice = device
	c.isRunning = true

	return nil
}

// StopAudioCapture stops capturing audio
func (c *Client) StopAudioCapture() error {
	c.mu.Lock()
	defer c.mu.Unlock()

	if !c.isRunning {
		return fmt.Errorf("audio capture is not running")
	}

	if c.audioDevice != nil {
		c.audioDevice.Stop()
		c.audioDevice.Uninit()
		c.audioDevice = nil
	}

	c.isRunning = false
	return nil
}

// HandleEvent handles a session event
func (c *Client) HandleEvent(event map[string]interface{}) {
	eventType, ok := event["event"].(string)
	if !ok {
		log.Printf("Invalid event: %v", event)
		return
	}

	switch eventType {
	case "answer":
		sdp, ok := event["sdp"].(string)
		if !ok {
			log.Printf("Invalid answer event: %v", event)
			return
		}
		err := c.SetRemoteDescription(sdp)
		if err != nil {
			log.Printf("Error setting remote description: %v", err)
		}
	case "hangup":
		log.Printf("Call hung up")
		c.Close()
	case "error":
		errorMsg, ok := event["error"].(string)
		if !ok {
			log.Printf("Invalid error event: %v", event)
			return
		}
		log.Printf("Error: %s", errorMsg)
		c.Close()
	default:
		log.Printf("Unhandled event: %s", eventType)
	}
}
