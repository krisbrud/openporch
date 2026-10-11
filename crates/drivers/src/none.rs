//! `openporch/none`: returns the resource's resolved inputs as outputs and
//! creates nothing, for workloads in environments without a runtime.

use crate::{
    DeleteOutcome, Driver, DriverFuture, DriverInfo, DriverOutputs, DriverRequest, Observation,
    Plan, PutOutcome, any_object, stateless,
};

/// Returns the resolved resource inputs as output values, and the inputs that
/// resolved from secrets as output secrets.
#[derive(Debug, Clone)]
pub struct NoneDriver {
    info: DriverInfo,
}

impl NoneDriver {
    pub fn new() -> Self {
        Self {
            info: DriverInfo {
                id: "openporch/none".into(),
                protocol_version: "builtin".into(),
                cookie_version: 0,
                input_schema: any_object(),
                output_schema: any_object(),
                capabilities: stateless(),
            },
        }
    }

    fn outputs(req: &DriverRequest) -> DriverOutputs {
        DriverOutputs {
            values: req.resource.clone(),
            secrets: req.resource_secrets.clone(),
            manifests: Vec::new(),
        }
    }
}

impl Default for NoneDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl Driver for NoneDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn plan<'a>(&'a self, _req: &'a DriverRequest) -> DriverFuture<'a, Plan> {
        Box::pin(async { Ok(Plan::NoEffects) })
    }

    fn put<'a>(&'a self, req: &'a DriverRequest) -> DriverFuture<'a, PutOutcome> {
        Box::pin(async move {
            Ok(PutOutcome::Ready {
                outputs: Self::outputs(req),
                cookie: None,
            })
        })
    }

    fn get<'a>(&'a self, req: &'a DriverRequest) -> DriverFuture<'a, Observation> {
        Box::pin(async move {
            Ok(Observation::Ready {
                outputs: Self::outputs(req),
                cookie: None,
            })
        })
    }

    fn delete<'a>(&'a self, _req: &'a DriverRequest) -> DriverFuture<'a, DeleteOutcome> {
        Box::pin(async { Ok(DeleteOutcome::Deleted) })
    }
}
