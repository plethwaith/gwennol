//! Bridges Gwead's [`SecretResolver`] to [`Operator::secret`].

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use gwead::kernel::secrets::{SecretError, SecretRequest, SecretResolver};
use gwead::serde_json::{Map, Value};

use crate::operator::Operator;

/// Asks the operator for each key a plugin's manifest declared, and only
/// those. The kernel intersects the answer with the declaration again, so
/// an over-eager operator cannot widen a plugin's bag either.
pub struct OperatorSecrets(pub Arc<dyn Operator>);

impl SecretResolver for OperatorSecrets {
    fn resolve<'a>(
        &'a self,
        request: SecretRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<Value, SecretError>> + Send + 'a>> {
        Box::pin(async move {
            let mut bag = Map::new();
            for key in request.declared_keys {
                if let Some(v) = self.0.secret(request.subject, key).await {
                    bag.insert(key.clone(), Value::String(v));
                }
            }
            Ok(Value::Object(bag))
        })
    }
}
