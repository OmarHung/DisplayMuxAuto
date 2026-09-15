use crate::{
    DisplayInput, DisplayMuxError, DisplayMuxProfile, MonitorControl, MonitorDescriptor,
    SwitchMode, SwitchOutcome,
};

pub struct DisplayMuxService<C> {
    controller: C,
    profile: DisplayMuxProfile,
}

impl<C: MonitorControl> DisplayMuxService<C> {
    pub fn new(controller: C, profile: DisplayMuxProfile) -> Self {
        Self {
            controller,
            profile,
        }
    }

    pub fn list_monitors(&self) -> Result<Vec<MonitorDescriptor>, DisplayMuxError> {
        self.controller.enumerate()
    }

    pub fn switch_to_input(
        &self,
        requested: DisplayInput,
        mode: SwitchMode,
    ) -> Result<SwitchOutcome, DisplayMuxError> {
        let target = self.find_exact_target()?;
        let current = self.controller.read_input(&target.id)?;

        if mode == SwitchMode::DryRun {
            return Ok(SwitchOutcome::DryRun {
                target,
                current,
                requested,
            });
        }

        if current == requested {
            return Ok(SwitchOutcome::AlreadySelected {
                target,
                input: current,
            });
        }

        self.controller.write_input(&target.id, requested)?;
        tracing::info!(
            monitor_id = target.id.as_str(),
            previous_input = current.value(),
            selected_input = requested.value(),
            "shared monitor input switched"
        );

        Ok(SwitchOutcome::Switched {
            target,
            previous: current,
            selected: requested,
        })
    }

    fn find_exact_target(&self) -> Result<MonitorDescriptor, DisplayMuxError> {
        let matches = self
            .controller
            .enumerate()?
            .into_iter()
            .filter(|monitor| {
                self.profile
                    .shared_monitor
                    .matches_exactly(&monitor.fingerprint)
            })
            .collect::<Vec<_>>();

        match matches.len() {
            0 => Err(DisplayMuxError::TargetNotFound),
            1 => Ok(matches.into_iter().next().expect("length checked")),
            count => Err(DisplayMuxError::AmbiguousTarget { count }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::HashMap};

    use super::*;
    use crate::{DisplayInput, MonitorFingerprint, MonitorId};

    struct FakeController {
        monitors: Vec<MonitorDescriptor>,
        inputs: HashMap<MonitorId, DisplayInput>,
        writes: RefCell<Vec<(MonitorId, DisplayInput)>>,
    }

    impl MonitorControl for FakeController {
        fn enumerate(&self) -> Result<Vec<MonitorDescriptor>, DisplayMuxError> {
            Ok(self.monitors.clone())
        }

        fn read_input(&self, monitor: &MonitorId) -> Result<DisplayInput, DisplayMuxError> {
            self.inputs.get(monitor).copied().ok_or_else(|| {
                DisplayMuxError::MonitorNoLongerAvailable(monitor.as_str().to_owned())
            })
        }

        fn supported_inputs(
            &self,
            _monitor: &MonitorId,
        ) -> Result<Vec<DisplayInput>, DisplayMuxError> {
            Ok(vec![input(0x0f), input(0x11)])
        }

        fn write_input(
            &self,
            monitor: &MonitorId,
            input: DisplayInput,
        ) -> Result<(), DisplayMuxError> {
            self.writes.borrow_mut().push((monitor.clone(), input));
            Ok(())
        }
    }

    fn input(value: u32) -> DisplayInput {
        DisplayInput::new(value).expect("test input is valid")
    }

    fn fingerprint(vendor: &str, product: &str, serial: &str) -> MonitorFingerprint {
        MonitorFingerprint::new(vendor, product, Some(serial))
    }

    fn monitor(id: &str, vendor: &str, product: &str, serial: &str) -> MonitorDescriptor {
        MonitorDescriptor {
            id: MonitorId::new(id),
            name: id.to_owned(),
            fingerprint: fingerprint(vendor, product, serial),
            active: true,
            built_in: false,
            max_resolution: None,
            resolution_source: None,
            connection: None,
        }
    }

    fn profile() -> DisplayMuxProfile {
        DisplayMuxProfile {
            shared_monitor: fingerprint("ACM", "1234", "SERIAL-1"),
        }
    }

    #[test]
    fn switches_only_the_exact_selected_target() {
        let selected = monitor("selected", "ACM", "1234", "SERIAL-1");
        let protected = monitor("protected", "OTH", "5678", "SERIAL-2");
        let controller = FakeController {
            monitors: vec![protected.clone(), selected.clone()],
            inputs: HashMap::from([(selected.id.clone(), input(0x0f))]),
            writes: RefCell::new(Vec::new()),
        };
        let service = DisplayMuxService::new(controller, profile());

        let outcome = service
            .switch_to_input(input(0x11), SwitchMode::Apply)
            .expect("switch succeeds");

        assert_eq!(
            outcome,
            SwitchOutcome::Switched {
                target: selected.clone(),
                previous: input(0x0f),
                selected: input(0x11),
            }
        );
        assert_eq!(
            service.controller.writes.borrow().as_slice(),
            &[(selected.id, input(0x11))]
        );
        assert!(!service
            .controller
            .writes
            .borrow()
            .iter()
            .any(|(id, _)| id == &protected.id));
    }

    #[test]
    fn dry_run_never_writes() {
        let asus = monitor("selected", "ACM", "1234", "SERIAL-1");
        let controller = FakeController {
            monitors: vec![asus.clone()],
            inputs: HashMap::from([(asus.id, input(0x0f))]),
            writes: RefCell::new(Vec::new()),
        };
        let service = DisplayMuxService::new(controller, profile());

        let outcome = service
            .switch_to_input(input(0x11), SwitchMode::DryRun)
            .expect("dry-run succeeds");

        assert!(matches!(outcome, SwitchOutcome::DryRun { .. }));
        assert!(service.controller.writes.borrow().is_empty());
    }

    #[test]
    fn missing_or_mismatched_serial_fails_closed() {
        let wrong = monitor("selected", "ACM", "1234", "different");
        let controller = FakeController {
            monitors: vec![wrong],
            inputs: HashMap::new(),
            writes: RefCell::new(Vec::new()),
        };
        let service = DisplayMuxService::new(controller, profile());

        assert_eq!(
            service.switch_to_input(input(0x11), SwitchMode::Apply),
            Err(DisplayMuxError::TargetNotFound)
        );
        assert!(service.controller.writes.borrow().is_empty());
    }

    #[test]
    fn ambiguous_matches_fail_closed() {
        let first = monitor("selected-1", "ACM", "1234", "SERIAL-1");
        let second = monitor("selected-2", "ACM", "1234", "SERIAL-1");
        let controller = FakeController {
            monitors: vec![first, second],
            inputs: HashMap::new(),
            writes: RefCell::new(Vec::new()),
        };
        let service = DisplayMuxService::new(controller, profile());

        assert_eq!(
            service.switch_to_input(input(0x11), SwitchMode::Apply),
            Err(DisplayMuxError::AmbiguousTarget { count: 2 })
        );
        assert!(service.controller.writes.borrow().is_empty());
    }

    #[test]
    fn already_selected_input_is_not_written_again() {
        let asus = monitor("selected", "ACM", "1234", "SERIAL-1");
        let controller = FakeController {
            monitors: vec![asus.clone()],
            inputs: HashMap::from([(asus.id.clone(), input(0x0f))]),
            writes: RefCell::new(Vec::new()),
        };
        let service = DisplayMuxService::new(controller, profile());

        let outcome = service
            .switch_to_input(input(0x0f), SwitchMode::Apply)
            .expect("no-op succeeds");

        assert_eq!(
            outcome,
            SwitchOutcome::AlreadySelected {
                target: asus,
                input: input(0x0f),
            }
        );
        assert!(service.controller.writes.borrow().is_empty());
    }
}
