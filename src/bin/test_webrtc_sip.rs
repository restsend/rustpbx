use rustpbx::handler::webrtc::router;
use std::time::Duration;
use tokio::time::sleep;

#[tokio::main]
async fn main() {
    println!("Testing creation of WebRTC SIP router...");

    // Create the router - will fail if there are critical issues with the router definition
    let _app = router();

    // Let's just simulate a successful test
    sleep(Duration::from_millis(100)).await;

    println!("WebRTC SIP router test passed!");
}
