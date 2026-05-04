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

        if !self.legs.contains_key(&supervisor_leg) {
            return Err(anyhow!("Supervisor leg not found: {}", supervisor_leg));
        }
        let resolved_target_leg = self.resolve_supervisor_target(&target_leg)?;

        let conf_id = format!("supervisor-{}-listen", self.id.0);
        self.ensure_supervisor_conference(&conf_id).await?;

        let target_participant_leg = LegId::new(format!("{}-{}", self.id.0, resolved_target_leg));
        self.start_conference_media_bridge(&conf_id, &target_participant_leg)
            .await?;

        let supervisor_participant_leg = LegId::new(format!("{}-{}", self.id.0, supervisor_leg));
        self.start_conference_media_bridge(&conf_id, &supervisor_participant_leg)
            .await?;

        self.conference_bridge.conf_id = Some(conf_id);

        self.update_leg_state(&supervisor_leg, LegState::Connected);
        info!(
            session_id = %self.id,
            supervisor = %supervisor_leg,
            target = %resolved_target_leg,
            "Supervisor listen mode activated via conference bridge"
        );
        Ok(())
    }

    pub(super) fn resolve_supervisor_target(&self, target_leg: &LegId) -> Result<LegId> {
        if self.legs.contains_key(target_leg) {
            Ok(target_leg.clone())
        } else if self.legs.contains_key(&LegId::new("callee")) {
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

    pub(super) async fn ensure_supervisor_conference(&self, conf_id: &str) -> Result<()> {
        let conf_id_obj = crate::call::runtime::ConferenceId::from(conf_id);
        if self
            .server
            .conference_manager
            .get_conference(&conf_id_obj)
            .await
            .is_none()
        {
            info!(conf_id = %conf_id, "Creating supervisor conference");
            self.server
                .conference_manager
                .create_conference(conf_id_obj, Some(3))
                .await
                .map_err(|e| anyhow!("Failed to create supervisor conference: {}", e))?;
        }
        Ok(())
    }

    pub(super) async fn handle_supervisor_whisper(
        &mut self,
        supervisor_leg: LegId,
        target_leg: LegId,
        _supervisor_session_id: Option<String>,
    ) -> Result<()> {
        if !self.legs.contains_key(&supervisor_leg) {
            return Err(anyhow!("Supervisor leg not found: {}", supervisor_leg));
        }
        if !self.legs.contains_key(&target_leg) {
            return Err(anyhow!("Target leg not found: {}", target_leg));
        }

        let conf_id = format!("supervisor-{}-whisper", self.id.0);
        self.ensure_supervisor_conference(&conf_id).await?;

        let target_participant_leg = LegId::new(format!("{}-{}", self.id.0, target_leg));
        self.start_conference_media_bridge(&conf_id, &target_participant_leg)
            .await?;

        let supervisor_participant_leg = LegId::new(format!("{}-{}", self.id.0, supervisor_leg));
        self.start_conference_media_bridge(&conf_id, &supervisor_participant_leg)
            .await?;

        self.conference_bridge.conf_id = Some(conf_id);

        self.update_leg_state(&supervisor_leg, LegState::Connected);
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
        if !self.legs.contains_key(&supervisor_leg) {
            return Err(anyhow!("Supervisor leg not found: {}", supervisor_leg));
        }
        if !self.legs.contains_key(&target_leg) {
            return Err(anyhow!("Target leg not found: {}", target_leg));
        }

        let conf_id = format!("supervisor-{}-barge", self.id.0);
        self.ensure_supervisor_conference(&conf_id).await?;

        let leg_ids: Vec<LegId> = self.legs.keys().cloned().collect();

        for leg_id in &leg_ids {
            let participant_leg = LegId::new(format!("{}-{}", self.id.0, leg_id));
            if let Err(e) = self
                .start_conference_media_bridge(&conf_id, &participant_leg)
                .await
            {
                warn!(%leg_id, error = %e, "Failed to bridge leg into barge conference");
            }
        }

        let supervisor_participant_leg = LegId::new(format!("{}-{}", self.id.0, supervisor_leg));
        self.start_conference_media_bridge(&conf_id, &supervisor_participant_leg)
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
        let resolved_target_leg = if self.legs.contains_key(&target_leg) {
            target_leg
        } else if self.legs.contains_key(&LegId::new("callee")) {
            warn!(
                session_id = %self.id,
                requested_leg = %target_leg,
                "Cross-session supervisor listen target leg not found, falling back to callee"
            );
            LegId::new("callee")
        } else if self.legs.contains_key(&LegId::new("caller")) {
            warn!(
                session_id = %self.id,
                requested_leg = %target_leg,
                "Cross-session supervisor listen target leg not found, falling back to caller"
            );
            LegId::new("caller")
        } else {
            return Err(anyhow!("Target leg not found: {}", target_leg));
        };

        let conf_id = format!("supervisor-{}-{}", self.id.0, supervisor_session_id);
        let conf_id_obj = crate::call::runtime::ConferenceId::from(conf_id.as_str());

        if self
            .server
            .conference_manager
            .get_conference(&conf_id_obj)
            .await
            .is_none()
        {
            info!(conf_id = %conf_id, "Creating supervisor conference");
            self.server
                .conference_manager
                .create_conference(conf_id_obj.clone(), Some(3))
                .await
                .map_err(|e| anyhow!("Failed to create supervisor conference: {}", e))?;
        }

        let target_participant_leg = LegId::new(format!("{}-{}", self.id.0, resolved_target_leg));
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
                self.conference_bridge.bridge_handle = Some(handle);
                self.conference_bridge.conf_id = Some(conf_id.clone());
            }
            Err(e) => {
                return Err(anyhow!(
                    "Failed to start supervisor conference media bridge for target {}: {}",
                    resolved_target_leg,
                    e
                ));
            }
        }

        let registry = &self.server.active_call_registry;
        if let Some(handle) = registry.get_handle(supervisor_session_id) {
            let join_cmd = CallCommand::JoinMixer {
                mixer_id: conf_id.clone(),
            };
            handle
                .send_command(join_cmd)
                .map_err(|e| anyhow!("Failed to notify supervisor session: {}", e))?;
            info!(
                supervisor_session = %supervisor_session_id,
                conf_id = %conf_id,
                "Notified supervisor session to join conference"
            );
        } else {
            return Err(anyhow!(
                "Supervisor session {} not found",
                supervisor_session_id
            ));
        }

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
        if !self.legs.contains_key(&supervisor_leg) {
            return Err(anyhow!("Supervisor leg not found: {}", supervisor_leg));
        }
        if !self.legs.contains_key(&target_leg) {
            return Err(anyhow!("Target leg not found: {}", target_leg));
        }

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
        if !self.legs.contains_key(&supervisor_leg) {
            return Err(anyhow!("Supervisor leg not found: {}", supervisor_leg));
        }

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
