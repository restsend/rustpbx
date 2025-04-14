package main

import (
	"fmt"
	"log"
	"sync"

	"github.com/gen2brain/malgo"
)

// AudioDevice represents an audio device
type AudioDevice struct {
	ID          string
	Name        string
	IsInput     bool
	IsDefault   bool
	SampleRate  uint32
	Channels    uint32
	Format      malgo.FormatType
	DeviceIndex uint32
}

// DeviceManager manages audio devices
type DeviceManager struct {
	context         *malgo.AllocatedContext
	devices         []AudioDevice
	inputDevice     *AudioDevice
	mu              sync.Mutex
	captureMutex    sync.Mutex
	playbackMutex   sync.Mutex
	captureRunning  bool
	playbackRunning bool
	captureDevice   *malgo.Device
	playbackDevice  *malgo.Device
	audioQueue      chan []byte
	logger          *log.Logger
}

// NewDeviceManager creates a new device manager
func NewDeviceManager() (*DeviceManager, error) {
	ctx, err := malgo.InitContext(nil, malgo.ContextConfig{}, func(message string) {
		fmt.Printf("MALGO LOG: %s\n", message)
	})
	if err != nil {
		return nil, fmt.Errorf("failed to initialize malgo context: %w", err)
	}

	dm := &DeviceManager{
		context:    ctx,
		devices:    make([]AudioDevice, 0),
		audioQueue: make(chan []byte, 100),
		logger:     log.New(log.Writer(), "[DeviceManager] ", log.LstdFlags),
	}

	err = dm.refreshDevices()
	if err != nil {
		return nil, fmt.Errorf("failed to refresh devices: %w", err)
	}

	return dm, nil
}

// Close closes the device manager
func (dm *DeviceManager) Close() {
	if dm.context != nil {
		dm.context.Uninit()
	}
}

// refreshDevices refreshes the list of devices
func (dm *DeviceManager) refreshDevices() error {
	dm.mu.Lock()
	defer dm.mu.Unlock()

	dm.devices = make([]AudioDevice, 0)

	// Get playback devices
	playbackDevices, err := dm.context.Devices(malgo.Playback)
	if err != nil {
		return fmt.Errorf("failed to get playback devices: %w", err)
	}

	for i, device := range playbackDevices {
		info := malgo.DefaultDeviceConfig(malgo.Playback)
		dm.devices = append(dm.devices, AudioDevice{
			ID:          device.ID.String(),
			Name:        device.Name(),
			IsInput:     false,
			IsDefault:   device.IsDefault == 1,
			SampleRate:  info.SampleRate,
			Channels:    uint32(info.Playback.Channels),
			Format:      info.Playback.Format,
			DeviceIndex: uint32(i),
		})
	}

	// Get capture devices
	captureDevices, err := dm.context.Devices(malgo.Capture)
	if err != nil {
		return fmt.Errorf("failed to get capture devices: %w", err)
	}

	for i, device := range captureDevices {
		info := malgo.DefaultDeviceConfig(malgo.Capture)
		audioDevice := AudioDevice{
			ID:          device.ID.String(),
			Name:        device.Name(),
			IsInput:     true,
			IsDefault:   device.IsDefault == 1,
			SampleRate:  info.SampleRate,
			Channels:    uint32(info.Capture.Channels),
			Format:      info.Capture.Format,
			DeviceIndex: uint32(i),
		}
		dm.devices = append(dm.devices, audioDevice)

		// Set as default input device if it's the default
		if device.IsDefault == 1 {
			dm.inputDevice = &audioDevice
		}
	}

	return nil
}

// GetDevices returns all devices
func (dm *DeviceManager) GetDevices() []AudioDevice {
	dm.mu.Lock()
	defer dm.mu.Unlock()
	return dm.devices
}

// GetInputDevices returns all input devices
func (dm *DeviceManager) GetInputDevices() []AudioDevice {
	dm.mu.Lock()
	defer dm.mu.Unlock()

	inputDevices := make([]AudioDevice, 0)
	for _, device := range dm.devices {
		if device.IsInput {
			inputDevices = append(inputDevices, device)
		}
	}
	return inputDevices
}

// GetDefaultInputDevice returns the default input device
func (dm *DeviceManager) GetDefaultInputDevice() *AudioDevice {
	dm.mu.Lock()
	defer dm.mu.Unlock()
	return dm.inputDevice
}

// GetDeviceByID returns a device by ID
func (dm *DeviceManager) GetDeviceByID(id string) *AudioDevice {
	dm.mu.Lock()
	defer dm.mu.Unlock()

	for i := range dm.devices {
		if dm.devices[i].ID == id {
			return &dm.devices[i]
		}
	}
	return nil
}

func (dm *DeviceManager) startCapture() error {
	dm.captureMutex.Lock()
	if dm.captureRunning {
		dm.captureMutex.Unlock()
		return nil
	}
	dm.captureRunning = true

	// Get the default input device
	inputDevice := dm.GetDefaultInputDevice()
	if inputDevice == nil {
		dm.captureRunning = false
		dm.captureMutex.Unlock()
		return fmt.Errorf("no input device found")
	}

	// Create a new audio device
	deviceConfig := malgo.DefaultDeviceConfig(malgo.Capture)
	deviceConfig.Capture.Format = malgo.FormatS16
	deviceConfig.Capture.Channels = 1
	deviceConfig.SampleRate = 16000
	deviceConfig.Alsa.NoMMap = 1

	// Create a callback function to handle audio data
	callbacks := malgo.DeviceCallbacks{
		Data: func(outputSamples, inputSamples []byte, framecount uint32) {
			// Process the audio data
			dm.processAudioData(inputSamples)
		},
	}

	// Create a new audio device
	device, err := malgo.InitDevice(dm.context.Context, deviceConfig, callbacks)
	if err != nil {
		dm.captureRunning = false
		dm.captureMutex.Unlock()
		return fmt.Errorf("failed to initialize audio device: %w", err)
	}

	// Start the audio device
	err = device.Start()
	if err != nil {
		dm.captureRunning = false
		dm.captureMutex.Unlock()
		return fmt.Errorf("failed to start audio device: %w", err)
	}

	dm.captureDevice = device
	dm.captureMutex.Unlock()

	return nil
}

func (dm *DeviceManager) startPlayback() error {
	dm.playbackMutex.Lock()
	if dm.playbackRunning {
		dm.playbackMutex.Unlock()
		return nil
	}
	dm.playbackRunning = true

	// Get the default output device
	outputDevice := dm.GetDefaultOutputDevice()
	if outputDevice == nil {
		dm.playbackRunning = false
		dm.playbackMutex.Unlock()
		return fmt.Errorf("no output device found")
	}

	// Create a new audio device
	deviceConfig := malgo.DefaultDeviceConfig(malgo.Playback)
	deviceConfig.Playback.Format = malgo.FormatS16
	deviceConfig.Playback.Channels = 1
	deviceConfig.SampleRate = 16000
	deviceConfig.Alsa.NoMMap = 1

	// Create a callback function to handle audio data
	callbacks := malgo.DeviceCallbacks{
		Data: func(outputSamples, inputSamples []byte, framecount uint32) {
			// Get audio data from the queue
			select {
			case data := <-dm.audioQueue:
				// Copy the audio data to the output buffer
				copy(outputSamples, data)
			default:
				// No data available, fill with silence
				for i := range outputSamples {
					outputSamples[i] = 0
				}
			}
		},
	}

	// Create a new audio device
	device, err := malgo.InitDevice(dm.context.Context, deviceConfig, callbacks)
	if err != nil {
		dm.playbackRunning = false
		dm.playbackMutex.Unlock()
		return fmt.Errorf("failed to initialize audio device: %w", err)
	}

	// Start the audio device
	err = device.Start()
	if err != nil {
		dm.playbackRunning = false
		dm.playbackMutex.Unlock()
		return fmt.Errorf("failed to start audio device: %w", err)
	}

	dm.playbackDevice = device
	dm.playbackMutex.Unlock()

	return nil
}

func (dm *DeviceManager) processAudioData(data []byte) {
	// Send the audio data to the queue for playback
	select {
	case dm.audioQueue <- data:
	default:
		// Queue is full, drop the data
		dm.logger.Printf("Audio queue is full, dropping data")
	}
}

// GetDefaultOutputDevice returns the default output device
func (dm *DeviceManager) GetDefaultOutputDevice() *AudioDevice {
	dm.mu.Lock()
	defer dm.mu.Unlock()

	for i := range dm.devices {
		if !dm.devices[i].IsInput && dm.devices[i].IsDefault {
			return &dm.devices[i]
		}
	}
	return nil
}
