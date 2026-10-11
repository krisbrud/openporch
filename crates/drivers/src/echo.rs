//! `openporch/echo`: returns the definition's driver inputs as outputs, for
//! resources managed outside openporch.

use crate::{
    DeleteOutcome, Driver, DriverFuture, DriverInfo, DriverOutputs, DriverRequest, Observation,
    Plan, PutOutcome, any_object, stateless,
};

/// Returns `driver_inputs.values` as output values and `driver_inputs.secrets` as
/// output secrets, without external calls.
#[derive(Debug, Clone)]
pub struct Echo {
    info: DriverInfo,
}

impl Echo {
    pub fn new() -> Self {
        Self {
            info: DriverInfo {
                id: "openporch/echo".into(),
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
            values: req.values.clone(),
            secrets: req.secrets.clone(),
            manifests: Vec::new(),
        }
    }
}

impl Default for Echo {
    fn default() -> Self {
        Self::new()
    }
}

impl Driver for Echo {
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
