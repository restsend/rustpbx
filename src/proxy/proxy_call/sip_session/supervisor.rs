use super::SipSession;
use crate::call::domain::{CallCommand, LegId, LegState};
use crate::call::runtime::BridgeConfig;
use anyhow::{Result, anyhow};
use tracing::{info, warn};

impl SipSession {
    pub(super) async fn handle_supervisor_listen(
        &mut self,
        supervisor_leg: LegId,
        target_leg: LegId,
        supervisor_session_id: Option<String>,
    ) -> Result<()> {
        if let Some(ref sup_session_id) = supervisor_session_id
            && sup_session_id != &self.id.0
        {
            return self
                .handle_cross_session_supervisor_listen(sup_session_id, target_leg)
                .await;
        }

        self.require_leg(&supervisor_leg)?;
        let resolved_target_leg = self.resolve_supervisor_target(&target_leg, false)?;
        self.start_supervisor_bridge_pair("listen", &supervisor_leg, &resolved_target_leg)
            .await?;
        info!(
            session_id = %self.id,
            supervisor = %supervisor_leg,
            target = %resolved_target_leg,
            "Supervisor listen mode activated via conference bridge"
        );
        Ok(())
    }

    /// Resolve a supervisor target leg, optionally skipping the exact match
    /// (used by cross-session listen where the target_leg may be a session id).
    /// Falls back to "callee" then "caller".
    pub(super) fn resolve_supervisor_target(
        &self,
        target_leg: &LegId,
        skip_exact: bool,
    ) -> Result<LegId> {
        if !skip_exact && self.legs.contains_key(target_leg) {
            return Ok(target_leg.clone());
        }
        if self.legs.contains_key(&LegId::new("callee")) {
            warn!(
                session_id = %self.id,
                requested_leg = %target_leg,
                "Supervisor target leg not found, falling back to callee"
            );
            Ok(LegId::new("callee"))
        } else if self.legs.contains_key(&LegId::new("caller")) {
            warn!(
                session_id = %self.id,
                requested_leg = %target_leg,
                "Supervisor target leg not found, falling back to caller"
            );
            Ok(LegId::new("caller"))
        } else {
            Err(anyhow!("Target leg not found: {}", target_leg))
        }
    }

    /// Common body for listen/whisper: ensure conference, bridge both legs,
    /// store conf_id and update supervisor leg state.
    async fn start_supervisor_bridge_pair(
        &mut self,
        kind: &str,
        supervisor_leg: &LegId,
        target_leg: &LegId,
    ) -> Result<()> {
        let conf_id = format!("supervisor-{}-{}", self.id.0, kind);
        self.ensure_conference(&conf_id, Some(3)).await?;

        let target_handle = self
            .start_conference_media_bridge(&conf_id, &self.participant_leg(target_leg))
            .await?;
        self.legs
            .set_conference_bridge_handle(target_leg.clone(), target_handle);

        let sup_handle = self
            .start_conference_media_bridge(&conf_id, &self.participant_leg(supervisor_leg))
            .await?;
        self.legs
            .set_conference_bridge_handle(supervisor_leg.clone(), sup_handle);

        self.conference_bridge.conf_id = Some(conf_id);
        self.update_leg_state(supervisor_leg, LegState::Connected);
        Ok(())
    }

    pub(super) async fn handle_supervisor_whisper(
        &mut self,
        supervisor_leg: LegId,
        target_leg: LegId,
        _supervisor_session_id: Option<String>,
    ) -> Result<()> {
        self.require_leg(&supervisor_leg)?;
        self.require_leg(&target_leg)?;
        self.start_supervisor_bridge_pair("whisper", &supervisor_leg, &target_leg)
            .await?;
        info!(
            session_id = %self.id,
            supervisor = %supervisor_leg,
            target = %target_leg,
            "Supervisor whisper mode activated via conference bridge"
        );
        Ok(())
    }

    pub(super) async fn handle_supervisor_barge(
        &mut self,
        supervisor_leg: LegId,
        target_leg: LegId,
        _supervisor_session_id: Option<String>,
    ) -> Result<()> {
        self.require_leg(&supervisor_leg)?;
        self.require_leg(&target_leg)?;

        let conf_id = format!("supervisor-{}-barge", self.id.0);
        self.ensure_conference(&conf_id, Some(3)).await?;

        let leg_ids: Vec<LegId> = self.legs.keys().cloned().collect();

        for leg_id in &leg_ids {
            let participant_leg = self.participant_leg(leg_id);
            if let Err(e) = self
                .start_conference_media_bridge(&conf_id, &participant_leg)
                .await
            {
                warn!(%leg_id, error = %e, "Failed to bridge leg into barge conference");
            }
        }

        self.start_conference_media_bridge(&conf_id, &self.participant_leg(&supervisor_leg))
            .await?;
        self.conference_bridge.conf_id = Some(conf_id);

        self.update_leg_state(&supervisor_leg, LegState::Connected);
        info!(
            session_id = %self.id,
            supervisor = %supervisor_leg,
            target = %target_leg,
            "Supervisor barge mode activated via conference bridge"
        );
        Ok(())
    }

    pub(super) async fn handle_cross_session_supervisor_listen(
        &mut self,
        supervisor_session_id: &str,
        target_leg: LegId,
    ) -> Result<()> {
        // Ignore a target_leg that matches the session ID itself (this can happen
        // when the caller passes the session ID as the target leg) and prefer the
        // real "callee" or "caller" leg instead.
        let target_is_session_id = target_leg.0 == self.id.0;
        let resolved_target_leg =
            self.resolve_supervisor_target(&target_leg, target_is_session_id)?;

        let conf_id = format!("supervisor-{}-{}", self.id.0, supervisor_session_id);
        self.ensure_conference(&conf_id, Some(3)).await?;

        let target_participant_leg = self.participant_leg(&resolved_target_leg);
        match self
            .start_conference_media_bridge(&conf_id, &target_participant_leg)
            .await
        {
            Ok(handle) => {
                info!(
                    session_id = %self.id,
                    leg_id = %resolved_target_leg,
                    "Supervisor conference media bridge started for target"
                );
                self.set_active_bridge(conf_id.clone(), handle);
            }
            Err(e) => {
                return Err(anyhow!(
                    "Failed to start supervisor conference media bridge for target {}: {}",
                    resolved_target_leg,
                    e
                ));
            }
        }

        self.forward_command(
            supervisor_session_id,
            CallCommand::JoinMixer {
                mixer_id: conf_id.clone(),
            },
            "notify supervisor session",
        )?;

        info!(
            session_id = %self.id,
            supervisor_session = %supervisor_session_id,
            conf_id = %conf_id,
            "Cross-session supervisor listen activated via conference"
        );
        Ok(())
    }

    pub(super) async fn handle_supervisor_takeover(
        &mut self,
        supervisor_leg: LegId,
        target_leg: LegId,
        _supervisor_session_id: Option<String>,
    ) -> Result<()> {
        self.require_leg(&supervisor_leg)?;
        self.require_leg(&target_leg)?;

        if let Some(ref mixer) = self.supervisor_mixer.take() {
            mixer.stop();
            info!(session_id = %self.id, "Stopped existing supervisor mixer for takeover");
        }

        let other_leg = if target_leg == LegId::new("caller") {
            LegId::new("callee")
        } else {
            LegId::new("caller")
        };

        self.bridge = BridgeConfig::bridge(supervisor_leg.clone(), other_leg.clone());

        self.update_leg_state(&target_leg, LegState::Ending);
        self.update_leg_state(&supervisor_leg, LegState::Connected);

        info!(
            session_id = %self.id,
            supervisor = %supervisor_leg,
            target = %target_leg,
            other = %other_leg,
            "Supervisor takeover activated"
        );
        Ok(())
    }

    pub(super) async fn handle_supervisor_stop(&mut self, supervisor_leg: LegId) -> Result<()> {
        self.require_leg(&supervisor_leg)?;

        if let Some(ref mixer) = self.supervisor_mixer {
            mixer.stop();
            info!(
                session_id = %self.id,
                "Supervisor mixer stopped"
            );
        }

        if self.legs.len() <= 2 {
            self.supervisor_mixer = None;
        }

        self.update_leg_state(&supervisor_leg, LegState::Ended);
        info!("Supervisor mode stopped");
        Ok(())
    }
}
