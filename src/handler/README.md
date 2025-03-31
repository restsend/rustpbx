# handler

## api
请求都是通过websocket完成, 发起通话之后,可以获得`SessionEvent`或者发送`command`
   - `/call/webrtc` 提供webrtc的offer sdp, 建立一个通话
   - `/call/sip` 发起一个sip呼叫,被叫是sip+rtp的方式建立通话
## call
call就是一个通话, 一个通话会构建一个`media::MediaStream`:
    - 根据呼叫的两端添加对应的`track`, 根据配置启用:`recorder`, `vad`, `asr`等processor
    - 是否启用`录音`功能, 如果启用录音,每个track的第一个`processor`就是录音的`RecorderProcssor`
    - `RecorderProcssor`会讲收到的音频转发给`Recorder`, 混音后存储到录音文件中
    - `AsrProcessor` 会根据配置启用不同的`TranscriptionClient`, 根据识别的结果,产生事件:`TranscriptionFinal`和`TranscriptionDelta`
    - `VadProcessor` 也会根据配置启用`webrtc`或者`silero`, 根据识别结果,产生对应的事件:`StartSpeaking`和`Silence`
    - 不同的`track` 对应不同的呼叫参与者:
        - webrtc 加密的,一般来着网页
        - rtp 普通的rtp连接,非加密, 一般是sip
        - file 播放本地的wav文件
        - tts 播放tts语音, 会根据配置调用`SynthesisClient`, 并且调用之前会先检查是否有缓存:
            - 缓存的key: 需要不同的SynthesisClient去计算: 应该包括samplerate, speakerid, lang, text等信息的md5, 并且文件名上应该加上8k/16k, zh_en 等信息
            - tts 如果合成的是wav, 需要转换成media相同`samplerate`的`pcm`
    - `command`的处理:
        - 连接创建之后,第一个请求必须是: `invite`:
            - webrtc: 应该提供sdp
            - sip: 需要提供callee
        - `tts` 播放一个语音,如果多段文字,可以指定相同的`play_id`, 如果`play_id`不同,会导致打断当前播放中的内容,播放新的内容
        - `play` 播放一个音频文件,会打断当前的播放
        - `hangup` 整个call停止
        - 当`tts`和`play`切换的时候, 会添加不同的`track`, 所以call要用一个通用的track_id: `callee`, 这样确保可以替换

## SessionEvent
订阅`call`的事件, 客户端可以根据事件做出操作就可以
## command
通过websocket发起命令,控制call的行为
```rust
pub enum Command {
    /// Invite a call
    #[serde(rename = "invite")]
    Invite {
        sdp: Option<String>,
        callee: Option<String>,
        caller: Option<String>,
    },
    /// Update the candidate for WebRTC
    #[serde(rename = "candidate")]
    Candidate { candidate: Vec<String> },
    /// Play a text to speech
    #[serde(rename = "tts")]
    Tts {
        text: String,
        speaker: Option<String>,
        play_id: Option<String>,
    },
    /// Play a wav file
    #[serde(rename = "play")]
    Play { url: String },
    /// Hangup the call
    #[serde(rename = "hangup")]
    Hangup {},
    /// Refer to a target
    #[serde(rename = "refer")]
    Refer { target: String },
    /// Mute a track
    #[serde(rename = "mute")]
    Mute { track_id: Option<String> },
    /// Unmute a track
    #[serde(rename = "unmute")]
    Unmute { track_id: Option<String> },
}
```