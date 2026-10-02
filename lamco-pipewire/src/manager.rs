//! Unified PipeWire Manager
//!
//! Provides a single entry point for PipeWire screen capture that hides
//! the internal thread architecture complexity.
//!
//! # Architecture
//!
//! The manager coordinates:
//! - Thread management (PipeWire requires dedicated thread for non-Send types)
//! - Stream lifecycle (creation, destruction, state changes)
//! - Frame delivery via channels
//! - Optional features (cursor extraction, damage tracking, adaptive bitrate)
//!
//! # Examples
//!
//! ```rust,ignore
//! use lamco_pipewire::{PipeWireManager, PipeWireConfig, StreamInfo, SourceType};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // Create manager with default config
//! let mut manager = PipeWireManager::with_default()?;
//!
//! // Connect using portal-provided FD
//! let fd = /* from lamco-portal */;
//! manager.connect(fd).await?;
//!
//! // Create stream for a monitor
//! let stream_info = StreamInfo {
//!     node_id: 42,
//!     position: (0, 0),
//!     size: (1920, 1080),
//!     source_type: SourceType::Monitor,
//! };
//!
//! let handle = manager.create_stream(&stream_info).await?;
//!
//! // Receive frames
//! let mut rx = manager.frame_receiver(handle.id).expect("receiver");
//! while let Some(frame) = rx.recv().await {
//!     println!("Frame: {}x{}", frame.width, frame.height);
//! }
//!
//! // Cleanup
//! manager.shutdown().await?;
//! # Ok(())
//! # }
//! ```

use std::collections::HashMap;
use std::os::fd::{IntoRawFd, OwnedFd, RawFd};
use std::sync::{Arc, mpsc as std_mpsc};

use tokio::sync::{Mutex, RwLock, mpsc};
use tracing::{debug, info, warn};

#[cfg(feature = "adaptive")]
use crate::bitrate::BitrateController;
use crate::config::PipeWireConfig;
use crate::coordinator::{SourceType, StreamInfo};
#[cfg(feature = "cursor")]
use crate::cursor::CursorExtractor;
#[cfg(feature = "damage")]
use crate::damage::DamageTracker;
use crate::error::{PipeWireError, Result};
use crate::frame::VideoFrame;
use crate::pw_thread::{PipeWireThreadCommand, PipeWireThreadManager};
use crate::stream::StreamConfig;

/// Handle to an active stream
#[derive(Debug, Clone)]
pub struct StreamHandle {
    /// Unique stream identifier
    pub id: u32,

    /// Node ID from portal
    pub node_id: u32,

    /// Stream position (for multi-monitor)
    pub position: (i32, i32),

    /// Stream dimensions
    pub size: (u32, u32),

    /// Source type
    pub source_type: SourceType,
}

/// Manager state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagerState {
    /// Manager created but not connected
    Disconnected,
    /// Connecting to PipeWire
    Connecting,
    /// Connected and ready
    Connected,
    /// Error state
    Error,
    /// Shutting down
    ShuttingDown,
}

/// Unified PipeWire manager
///
/// This is the primary entry point for PipeWire screen capture.
/// It manages the PipeWire thread, streams, and optional features.
pub struct PipeWireManager {
    /// Configuration
    config: PipeWireConfig,

    /// Manager state
    state: Arc<RwLock<ManagerState>>,

    /// Thread manager (handles PipeWire's non-Send types)
    thread_manager: Option<PipeWireThreadManager>,

    /// Active streams
    streams: Arc<Mutex<HashMap<u32, StreamHandle>>>,

    /// Frame receivers per stream (take-once: each receiver can only be retrieved once)
    frame_receivers: Arc<Mutex<HashMap<u32, mpsc::Receiver<VideoFrame>>>>,
    frame_senders: Arc<Mutex<HashMap<u32, mpsc::Sender<VideoFrame>>>>,

    /// Next stream ID
    next_stream_id: Arc<Mutex<u32>>,

    /// Portal file descriptor (raw copy for diagnostics; OwnedFd is consumed on connect)
    portal_fd: Option<RawFd>,
    // NOTE: We store RawFd here only for error context / diagnostics.
    // The actual OwnedFd is consumed by PipeWireThreadManager::new().
    /// Cursor extractor (if enabled)
    #[cfg(feature = "cursor")]
    cursor_extractor: Option<Arc<Mutex<CursorExtractor>>>,

    /// Damage tracker (if enabled)
    #[cfg(feature = "damage")]
    damage_tracker: Option<Arc<Mutex<DamageTracker>>>,

    /// Bitrate controller (if enabled)
    #[cfg(feature = "adaptive")]
    bitrate_controller: Option<Arc<Mutex<BitrateController>>>,
}

impl PipeWireManager {
    /// Create manager with default configuration
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// let manager = PipeWireManager::with_default()?;
    /// ```
    pub fn with_default() -> Result<Self> {
        Self::new(PipeWireConfig::default())
    }

    /// Create manager with custom configuration
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// use lamco_pipewire::{PipeWireManager, PipeWireConfig};
    ///
    /// let config = PipeWireConfig::builder()
    ///     .buffer_count(4)
    ///     .use_dmabuf(true)
    ///     .build();
    ///
    /// let manager = PipeWireManager::new(config)?;
    /// ```
    pub fn new(config: PipeWireConfig) -> Result<Self> {
        // Validate configuration
        if let Err(issues) = config.validate() {
            return Err(PipeWireError::InvalidParameter(issues.join(", ")));
        }

        info!("Creating PipeWireManager with config: {:?}", config);

        Ok(Self {
            config,
            state: Arc::new(RwLock::new(ManagerState::Disconnected)),
            thread_manager: None,
            streams: Arc::new(Mutex::new(HashMap::new())),
            frame_receivers: Arc::new(Mutex::new(HashMap::new())),
            frame_senders: Arc::new(Mutex::new(HashMap::new())),
            next_stream_id: Arc::new(Mutex::new(0)),
            portal_fd: None,
            #[cfg(feature = "cursor")]
            cursor_extractor: None,
            #[cfg(feature = "damage")]
            damage_tracker: None,
            #[cfg(feature = "adaptive")]
            bitrate_controller: None,
        })
    }

    /// Connect to PipeWire using portal-provided file descriptor
    ///
    /// The file descriptor should be obtained from XDG Desktop Portal
    /// (e.g., via `lamco-portal`).
    ///
    /// # Arguments
    ///
    /// * `fd` - File descriptor from portal's PipeWire connection
    ///
    /// # Errors
    ///
    /// Returns error if:
    /// - Already connected
    /// - PipeWire initialization fails
    /// - Connection timeout exceeded
    pub async fn connect(&mut self, fd: OwnedFd) -> Result<()> {
        let current_state = *self.state.read().await;
        if current_state == ManagerState::Connected {
            return Err(PipeWireError::InvalidState("Already connected".to_string()));
        }

        *self.state.write().await = ManagerState::Connecting;
        let raw_fd = fd.into_raw_fd();
        info!("Connecting to PipeWire with FD {}", raw_fd);

        self.portal_fd = Some(raw_fd);

        // Initialize PipeWire thread manager
        
        let mut thread_manager = PipeWireThreadManager::new(raw_fd)?;
        
        if let Some(frame_rx) = thread_manager.take_frame_rx() {
            let frame_notify = thread_manager.frame_notify();
            let senders = self.frame_senders.clone();
            
            tokio::spawn(async move {
                loop {
                    frame_notify.notified().await;
                    while let Ok(frame) = frame_rx.try_recv() {
                        let stream_id = frame.monitor_index;
                        let sender_opt = {
                            let map = senders.lock().await;
                            map.get(&stream_id).cloned()
                        };
                        if let Some(sender) = sender_opt {
                            let _ = sender.send(frame).await;
                        }
                    }
                }
            });
        }
        
        self.thread_manager = Some(thread_manager);


        // Initialize optional features
        #[cfg(feature = "cursor")]
        if self.config.enable_cursor {
            self.cursor_extractor = Some(Arc::new(Mutex::new(CursorExtractor::new())));
            debug!("Cursor extractor enabled");
        }

        #[cfg(feature = "damage")]
        if self.config.enable_damage_tracking {
            self.damage_tracker = Some(Arc::new(Mutex::new(DamageTracker::new())));
            debug!("Damage tracker enabled");
        }

        #[cfg(feature = "adaptive")]
        if let Some(ref adaptive_config) = self.config.adaptive_bitrate {
            self.bitrate_controller = Some(Arc::new(Mutex::new(BitrateController::new(adaptive_config.clone()))));
            debug!("Bitrate controller enabled");
        }

        *self.state.write().await = ManagerState::Connected;
        info!("PipeWire connected successfully");

        Ok(())
    }

    /// Create a stream for capturing from a source
    ///
    /// # Arguments
    ///
    /// * `stream_info` - Information about the source (from portal)
    ///
    /// # Returns
    ///
    /// Stream handle on success
    ///
    /// # Errors
    ///
    /// Returns error if:
    /// - Not connected
    /// - Maximum streams exceeded
    /// - Stream creation fails
    pub async fn create_stream(&mut self, stream_info: &StreamInfo) -> Result<StreamHandle> {
        if *self.state.read().await != ManagerState::Connected {
            return Err(PipeWireError::InvalidState("Not connected".to_string()));
        }

        // Check stream limit
        let stream_count = self.streams.lock().await.len();
        if stream_count >= self.config.max_streams {
            return Err(PipeWireError::TooManyStreams(self.config.max_streams));
        }

        // Generate stream ID
        let stream_id = {
            let mut id = self.next_stream_id.lock().await;
            let sid = *id;
            *id += 1;
            sid
        };

        info!(
            "Creating stream {} for node {} ({}x{} at {:?})",
            stream_id, stream_info.node_id, stream_info.size.0, stream_info.size.1, stream_info.position
        );

        // Create stream configuration
        let stream_name = format!("{}-{}", self.config.stream_name_prefix, stream_id);
        let stream_config = StreamConfig::new(stream_name)
            .with_resolution(stream_info.size.0, stream_info.size.1)
            .with_framerate(self.config.framerate)
            .with_fixed_size(self.config.fixed_size)
            .with_drive_graph(self.config.drive_graph)
            .with_ranged_buffers(self.config.ranged_buffers)
            .with_dmabuf(self.config.use_dmabuf)
            .with_buffer_count(self.config.buffer_count);

        // Create frame channel — receiver stored for consumer via frame_receiver()
        // TODO: wire sender to PW thread bridge for automatic frame routing
        let (tx, rx) = mpsc::channel(self.config.frame_buffer_size);
        self.frame_receivers.lock().await.insert(stream_id, rx);
        self.frame_senders.lock().await.insert(stream_id, tx);

        // Send command to PipeWire thread
        if let Some(ref thread_manager) = self.thread_manager {
            let (response_tx, response_rx) = std_mpsc::sync_channel(1);
            thread_manager.send_command(PipeWireThreadCommand::CreateStream {
                stream_id,
                node_id: stream_info.node_id,
                config: stream_config,
                response_tx,
            })?;

            // Wait for response from PipeWire thread
            response_rx
                .recv()
                .map_err(|_| {
                    PipeWireError::ThreadCommunicationFailed("CreateStream response channel closed".to_string())
                })?
                .map_err(|e| PipeWireError::StreamCreationFailed(format!("Stream creation failed: {}", e)))?;
        }

        // Create handle
        let handle = StreamHandle {
            id: stream_id,
            node_id: stream_info.node_id,
            position: stream_info.position,
            size: stream_info.size,
            source_type: stream_info.source_type,
        };

        self.streams.lock().await.insert(stream_id, handle.clone());

        info!("Stream {} created successfully", stream_id);
        Ok(handle)
    }

    /// Take frame receiver for a stream
    ///
    /// Returns the channel receiver for frames from the specified stream.
    /// This can only be called once per stream — subsequent calls return None.
    ///
    /// # Arguments
    ///
    /// * `stream_id` - ID of the stream
    ///
    /// # Returns
    ///
    /// Channel receiver for frames, or None if already taken or stream not found
    pub async fn frame_receiver(&self, stream_id: u32) -> Option<mpsc::Receiver<VideoFrame>> {
        self.frame_receivers.lock().await.remove(&stream_id)
    }

    /// Remove a stream
    ///
    /// Stops and removes the specified stream.
    ///
    /// # Arguments
    ///
    /// * `stream_id` - ID of the stream to remove
    pub async fn remove_stream(&mut self, stream_id: u32) -> Result<()> {
        info!("Removing stream {}", stream_id);

        if self.streams.lock().await.remove(&stream_id).is_none() {
            return Err(PipeWireError::StreamNotFound(stream_id));
        }

        self.frame_receivers.lock().await.remove(&stream_id);
        self.frame_senders.lock().await.remove(&stream_id);

        // Send command to PipeWire thread
        if let Some(ref thread_manager) = self.thread_manager {
            let (response_tx, response_rx) = std_mpsc::sync_channel(1);
            thread_manager.send_command(PipeWireThreadCommand::DestroyStream { stream_id, response_tx })?;

            // Wait for response (ignore errors during shutdown cleanup)
            if let Ok(result) = response_rx.recv() {
                result?;
            }
        }

        info!("Stream {} removed", stream_id);
        Ok(())
    }

    /// Get all active stream handles
    pub async fn streams(&self) -> Vec<StreamHandle> {
        self.streams.lock().await.values().cloned().collect()
    }

    /// Get stream by ID
    pub async fn stream(&self, stream_id: u32) -> Option<StreamHandle> {
        self.streams.lock().await.get(&stream_id).cloned()
    }

    /// Get current manager state
    pub async fn state(&self) -> ManagerState {
        *self.state.read().await
    }

    /// Check if connected
    pub async fn is_connected(&self) -> bool {
        *self.state.read().await == ManagerState::Connected
    }

    /// Get configuration
    pub fn config(&self) -> &PipeWireConfig {
        &self.config
    }

    /// Access cursor extractor (if enabled)
    #[cfg(feature = "cursor")]
    pub fn cursor_extractor(&self) -> Option<&Arc<Mutex<CursorExtractor>>> {
        self.cursor_extractor.as_ref()
    }

    /// Access damage tracker (if enabled)
    #[cfg(feature = "damage")]
    pub fn damage_tracker(&self) -> Option<&Arc<Mutex<DamageTracker>>> {
        self.damage_tracker.as_ref()
    }

    /// Access bitrate controller (if enabled)
    #[cfg(feature = "adaptive")]
    pub fn bitrate_controller(&self) -> Option<&Arc<Mutex<BitrateController>>> {
        self.bitrate_controller.as_ref()
    }

    /// Shutdown the manager
    ///
    /// Stops all streams and disconnects from PipeWire.
    pub async fn shutdown(&mut self) -> Result<()> {
        info!("Shutting down PipeWireManager");
        *self.state.write().await = ManagerState::ShuttingDown;

        // Remove all streams
        let stream_ids: Vec<u32> = self.streams.lock().await.keys().copied().collect();
        for id in stream_ids {
            if let Err(e) = self.remove_stream(id).await {
                warn!("Error removing stream {} during shutdown: {}", id, e);
            }
        }

        // Shutdown thread manager
        if let Some(ref thread_manager) = self.thread_manager {
            // Shutdown command doesn't need a response
            let _ = thread_manager.send_command(PipeWireThreadCommand::Shutdown);
        }

        self.thread_manager = None;
        *self.state.write().await = ManagerState::Disconnected;

        info!("PipeWireManager shutdown complete");
        Ok(())
    }
}

impl Drop for PipeWireManager {
    fn drop(&mut self) {
        debug!("Dropping PipeWireManager");
        // Thread manager handles its own cleanup in Drop
    }
}

/// Statistics for the manager
#[derive(Debug, Clone, Default)]
pub struct ManagerStats {
    /// Number of streams created
    pub streams_created: u64,

    /// Number of streams destroyed
    pub streams_destroyed: u64,

    /// Total frames processed
    pub total_frames: u64,

    /// Total bytes processed
    pub total_bytes: u64,

    /// Connection uptime in seconds
    pub uptime_secs: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_manager_creation() {
        let manager = PipeWireManager::with_default();
        assert!(manager.is_ok());
    }

    #[test]
    fn test_manager_with_config() {
        let config = PipeWireConfig::builder().buffer_count(5).max_streams(4).build();

        let manager = PipeWireManager::new(config);
        assert!(manager.is_ok());

        let mgr = manager.expect("manager should be created");
        assert_eq!(mgr.config().buffer_count, 5);
        assert_eq!(mgr.config().max_streams, 4);
    }

    #[test]
    fn test_invalid_config() {
        let config = PipeWireConfig {
            buffer_count: 0, // Invalid
            ..Default::default()
        };

        let manager = PipeWireManager::new(config);
        assert!(manager.is_err());
    }

    #[tokio::test]
    async fn test_manager_state() {
        let manager = PipeWireManager::with_default().expect("manager");
        assert_eq!(manager.state().await, ManagerState::Disconnected);
        assert!(!manager.is_connected().await);
    }

    #[test]
    fn test_stream_handle() {
        let handle = StreamHandle {
            id: 1,
            node_id: 42,
            position: (0, 0),
            size: (1920, 1080),
            source_type: SourceType::Monitor,
        };

        assert_eq!(handle.id, 1);
        assert_eq!(handle.node_id, 42);
        assert_eq!(handle.size, (1920, 1080));
    }
}
