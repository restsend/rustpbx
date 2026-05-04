#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RtpDtmfEventKey {
    pub(crate) digit_code: u8,
    pub(crate) rtp_timestamp: u32,
}

#[derive(Debug, Default)]
pub(crate) struct RtpDtmfDetector {
    pub(crate) last_event: Option<RtpDtmfEventKey>,
}

impl RtpDtmfDetector {
    pub(crate) fn observe(&mut self, payload: &[u8], rtp_timestamp: u32) -> Option<char> {
        if payload.len() < 4 {
            return None;
        }

        let digit_code = payload[0];
        let digit = crate::media::telephone_event::dtmf_code_to_char(digit_code)?;

        let event = RtpDtmfEventKey {
            digit_code,
            rtp_timestamp,
        };

        if self.last_event == Some(event) {
            return None;
        }

        self.last_event = Some(event);
        Some(digit)
    }
}
