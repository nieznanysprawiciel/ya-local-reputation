use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use structopt::StructOpt;
use tokio::runtime::Runtime;
use tokio::time::Instant;

use ya_client_model::NodeId;
use ya_negotiator_shared_lib_interface::plugin::{
    AgreementResult, AgreementView, NegotiationResult, NegotiatorComponent, NegotiatorConstructor,
    ProposalView, Score,
};
use ya_negotiator_shared_lib_interface::ya_negotiator_component::{AgreementEvent, RejectReason};
use ya_negotiator_shared_lib_interface::*;

/// Simple reputation blacklisting Node, when it doesn't pay
/// Invoice in specified timeout.
pub struct BlacklistReputation {
    config: Arc<BlacklistReputationsConfig>,
    state: Arc<Mutex<BlacklistState>>,
    runtime: Runtime,
    workdir: PathBuf,
}

pub struct BlacklistState {
    blacklist: Vec<NodeId>,
    agreements: HashMap<String, TrackedAgreement>,
}

pub struct TrackedAgreement {
    pub id: String,
    pub node: NodeId,
    pub signed: DateTime<Utc>,
    pub terminated: Option<Instant>,
}

#[derive(StructOpt, Serialize, Deserialize)]
pub struct BlacklistReputationsConfig {
    #[serde(with = "humantime_serde")]
    #[structopt(long, env, parse(try_from_str = humantime::parse_duration), default_value = "15s")]
    pub payment_timeout: std::time::Duration,
}

impl NegotiatorConstructor<BlacklistReputation> for BlacklistReputation {
    fn new(
        _name: &str,
        config: serde_yaml::Value,
        working_dir: PathBuf,
    ) -> anyhow::Result<BlacklistReputation> {
        let config: BlacklistReputationsConfig = serde_yaml::from_value(config)?;
        let runtime = Runtime::new()?;

        flexi_logger::Logger::try_with_env()?
            .log_to_file(
                flexi_logger::FileSpec::default()
                    .directory(&working_dir)
                    .basename("blacklist"),
            )
            .start()?;

        let blacklist = match fs::read_to_string(working_dir.join("blacklist.yaml")) {
            Ok(content) => serde_yaml::from_str(&content)?,
            Err(_) => vec![],
        };

        log::info!("Starting BlacklistReputation plugin.");

        Ok(BlacklistReputation {
            config: Arc::new(config),
            state: Arc::new(Mutex::new(BlacklistState {
                blacklist,
                agreements: Default::default(),
            })),
            runtime,
            workdir: working_dir,
        })
    }
}

impl Drop for BlacklistReputation {
    fn drop(&mut self) {
        let blacklist = {
            self.state
                .lock()
                .unwrap()
                .blacklist
                .drain(..)
                .collect::<Vec<NodeId>>()
        };

        if let Ok(content) = serde_yaml::to_string(&blacklist) {
            fs::write(self.workdir.join("blacklist.yaml"), content).ok();
        }
    }
}

impl NegotiatorComponent for BlacklistReputation {
    /// BlacklistReputation will reject any Node on blacklist.
    fn negotiate_step(
        &mut self,
        demand: &ProposalView,
        offer: ProposalView,
        score: Score,
    ) -> anyhow::Result<NegotiationResult> {
        if self
            .state
            .lock()
            .unwrap()
            .blacklist
            .contains(&demand.issuer)
        {
            log::info!("Rejecting blacklisted node: {}", demand.issuer);

            return Ok(NegotiationResult::Reject {
                reason: RejectReason::new("Node is blacklisted due to not paying Invoices."),
                is_final: true,
            });
        }

        log::debug!("Node {} allowed (not blacklisted).", demand.issuer);
        Ok(NegotiationResult::Ready {
            proposal: offer,
            score,
        })
    }

    /// Negotiator will expect Invoice to be paid in specified deadline after termination.
    /// We must store timestamp
    fn on_agreement_terminated(
        &mut self,
        agreement_id: &str,
        _result: &AgreementResult,
    ) -> anyhow::Result<()> {
        log::trace!("on_agreement_terminated [{}]", agreement_id);

        let mut state = self.state.lock().unwrap();
        if let Some(record) = state.agreements.get_mut(agreement_id) {
            let now = Instant::now();
            let state = self.state.clone();
            let deadline = now + self.config.payment_timeout;
            let agreement_id = agreement_id.to_string();

            record.terminated = Some(now);
            let node_id = record.node;

            log::debug!(
                "Setting timer ({}) for agreement [{}], node: {}",
                humantime::format_duration(self.config.payment_timeout),
                agreement_id,
                node_id
            );

            self.runtime.spawn(async move {
                tokio::time::sleep_until(deadline).await;

                let mut state = state.lock().unwrap();

                // If we don't find Agreement in the map, it have been paid.
                if let Some(record) = state.agreements.remove(&agreement_id) {
                    log::info!(
                        "Node [{}] didn't pay agreement [{}]. Blacklisting..",
                        node_id,
                        agreement_id
                    );
                    state.blacklist.push(record.node);
                }
            });
        }
        Ok(())
    }

    /// Store `Agreement` information and track it's state.
    fn on_agreement_approved(&mut self, agreement: &AgreementView) -> anyhow::Result<()> {
        log::trace!("on_agreement_approved [{}]", agreement.id);

        let record = TrackedAgreement {
            id: agreement.id.clone(),
            node: agreement.requestor_id()?,
            signed: agreement
                .pointer_typed::<DateTime<Utc>>("/approved_date")
                .unwrap_or(Utc::now()),
            terminated: None,
        };

        {
            self.state
                .lock()
                .unwrap()
                .agreements
                .insert(agreement.id.clone(), record);

            log::info!(
                "Registered agreement [{}] for node [{}].",
                agreement.id,
                agreement.requestor_id()?
            );
            Ok(())
        }
    }

    /// Notifies `NegotiatorComponent`, about events related to Agreement appearing after
    /// it's termination.
    fn on_agreement_event(
        &mut self,
        agreement_id: &str,
        event: &AgreementEvent,
    ) -> anyhow::Result<()> {
        log::trace!("on_agreement_event [{}]", agreement_id);

        let mut state = self.state.lock().unwrap();
        match event {
            AgreementEvent::InvoicePaid => {
                if let Some(record) = state.agreements.remove(agreement_id) {
                    log::info!(
                        "Node [{}] paid invoice for agreement [{}]. Stop tracking..",
                        record.node,
                        agreement_id
                    );
                }
                Ok(())
            }
            AgreementEvent::InvoiceRejected => {
                if let Some(record) = state.agreements.remove(agreement_id) {
                    log::info!(
                        "Node [{}] rejected invoice for agreement [{}]. Blacklisting..",
                        record.node,
                        agreement_id
                    );
                    state.blacklist.push(record.node)
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

register_negotiators!(BlacklistReputation);
