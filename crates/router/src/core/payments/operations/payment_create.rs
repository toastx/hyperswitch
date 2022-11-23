use std::marker::PhantomData;

use async_trait::async_trait;
use error_stack::ResultExt;
use router_derive::PaymentOperation;
use router_env::{instrument, tracing};
use uuid::Uuid;

use super::{BoxedOperation, Domain, GetTracker, Operation, UpdateTracker, ValidateRequest};
use crate::{
    consts,
    core::{
        errors::{self, RouterResult, StorageErrorExt},
        payments::{self, helpers, CustomerDetails, PaymentAddress, PaymentData},
        utils as core_utils,
    },
    db::{
        connector_response::IConnectorResponse, payment_attempt::IPaymentAttempt,
        payment_intent::IPaymentIntent, Db,
    },
    routes::AppState,
    types::{
        self, api,
        storage::{
            self,
            enums::{self, IntentStatus},
        },
    },
    utils::OptionExt,
};
#[derive(Debug, Clone, Copy, PaymentOperation)]
#[operation(ops = "all", flow = "authorize")]
pub struct PaymentCreate;

#[async_trait]
impl<F: Send + Clone> GetTracker<F, PaymentData<F>, api::PaymentsRequest> for PaymentCreate {
    #[instrument(skip_all)]
    async fn get_trackers<'a>(
        &'a self,
        state: &'a AppState,
        payment_id: &api::PaymentIdType,
        merchant_id: &str,
        connector: types::Connector,
        request: &api::PaymentsRequest,
        mandate_type: Option<api::MandateTxnType>,
    ) -> RouterResult<(
        BoxedOperation<'a, F, api::PaymentsRequest>,
        PaymentData<F>,
        Option<CustomerDetails>,
    )> {
        let db = &state.store;

        let (payment_intent, payment_attempt, connector_response);

        let money @ (amount, currency) = payments_create_request_validation(request)?;

        let mut is_update = false;

        let payment_id = payment_id
            .get_payment_intent_id()
            .change_context(errors::ApiErrorResponse::PaymentNotFound)?;

        let (token, payment_method_type, setup_mandate) =
            helpers::get_token_pm_type_mandate_details(state, request, mandate_type, merchant_id)
                .await?;

        let shipping_address =
            helpers::get_address_for_payment_request(db, request.shipping.as_ref(), None).await?;

        let billing_address =
            helpers::get_address_for_payment_request(db, request.billing.as_ref(), None).await?;

        payment_attempt = match db
            .insert_payment_attempt(Self::make_payment_attempt(
                &payment_id,
                merchant_id,
                connector,
                money,
                payment_method_type,
                request,
            ))
            .await
        {
            Ok(payment_attempt) => Ok(payment_attempt),

            Err(err) => match err.current_context() {
                errors::StorageError::DatabaseError(errors::DatabaseError::UniqueViolation) => {
                    is_update = true;
                    db.find_payment_attempt_by_payment_id_merchant_id(&payment_id, merchant_id)
                        .await
                        .map_err(|error| {
                            error.to_not_found_response(errors::ApiErrorResponse::PaymentNotFound)
                        })
                }
                _ => Err(err).change_context(errors::ApiErrorResponse::InternalServerError),
            },
        }?;
        payment_intent = match db
            .insert_payment_intent(Self::make_payment_intent(
                &payment_id,
                merchant_id,
                &connector.to_string(),
                money,
                request,
                shipping_address.clone().map(|x| x.address_id),
                billing_address.clone().map(|x| x.address_id),
            ))
            .await
        {
            Ok(payment_intent) => Ok(payment_intent),

            Err(err) => match err.current_context() {
                errors::StorageError::DatabaseError(errors::DatabaseError::UniqueViolation) => {
                    is_update = true;
                    db.find_payment_intent_by_payment_id_merchant_id(&payment_id, merchant_id)
                        .await
                        .map_err(|error| {
                            error.to_not_found_response(errors::ApiErrorResponse::PaymentNotFound)
                        })
                }
                _ => Err(err).change_context(errors::ApiErrorResponse::InternalServerError),
            },
        }?;

        connector_response = match db
            .insert_connector_response(Self::make_connector_response(&payment_attempt))
            .await
        {
            Ok(connector_resp) => Ok(connector_resp),
            Err(err) => match err.current_context() {
                errors::StorageError::DatabaseError(errors::DatabaseError::UniqueViolation) => {
                    Err(err)
                        .change_context(errors::ApiErrorResponse::InternalServerError)
                        .attach_printable("Duplicate connector response in the database")
                }
                _ => Err(err)
                    .change_context(errors::ApiErrorResponse::InternalServerError)
                    .attach_printable("Error occured when inserting connector response")?,
            },
        }?;

        let operation = payments::if_not_create_change_operation::<_, F>(
            is_update,
            payment_intent.status,
            self,
        );
        Ok((
            operation,
            PaymentData {
                flow: PhantomData,
                payment_intent,
                payment_attempt,
                currency,
                amount,
                mandate_id: request.mandate_id.clone(),
                setup_mandate,
                token,
                address: PaymentAddress {
                    shipping: shipping_address.as_ref().map(|a| a.into()),
                    billing: billing_address.as_ref().map(|a| a.into()),
                },
                confirm: request.confirm,
                payment_method_data: request.payment_method_data.clone(),
                refunds: vec![],
                force_sync: None,
                connector_response,
            },
            Some(CustomerDetails {
                customer_id: request.customer_id.clone(),
                name: request.name.clone(),
                email: request.email.clone(),
                phone: request.phone.clone(),
                phone_country_code: request.phone_country_code.clone(),
            }),
        ))
    }
}

#[async_trait]
impl<F: Clone> UpdateTracker<F, PaymentData<F>, api::PaymentsRequest> for PaymentCreate {
    #[instrument(skip_all)]
    async fn update_trackers<'b>(
        &'b self,
        db: &dyn Db,
        _payment_id: &api::PaymentIdType,
        mut payment_data: PaymentData<F>,
        _customer: Option<storage::Customer>,
    ) -> RouterResult<(BoxedOperation<'b, F, api::PaymentsRequest>, PaymentData<F>)>
    where
        F: 'b + Send,
    {
        let status = match payment_data.payment_intent.status {
            IntentStatus::RequiresPaymentMethod => match payment_data.payment_method_data {
                Some(_) => Some(IntentStatus::RequiresConfirmation),
                _ => None,
            },
            IntentStatus::RequiresConfirmation => {
                if let Some(true) = payment_data.confirm {
                    Some(IntentStatus::Processing)
                } else {
                    None
                }
            }
            _ => None,
        };

        let customer_id = payment_data.payment_intent.customer_id.clone();
        payment_data.payment_intent = db
            .update_payment_intent(
                payment_data.payment_intent,
                storage::PaymentIntentUpdate::ReturnUrlUpdate {
                    return_url: None,
                    status,
                    customer_id,
                    shipping_address_id: None,
                    billing_address_id: None,
                },
            )
            .await
            .map_err(|error| {
                error.to_not_found_response(errors::ApiErrorResponse::PaymentNotFound)
            })?;

        // payment_data.mandate_id = response.and_then(|router_data| router_data.request.mandate_id);

        Ok((
            payments::is_confirm(self, payment_data.confirm),
            payment_data,
        ))
    }
}

impl<F: Send + Clone> ValidateRequest<F, api::PaymentsRequest> for PaymentCreate {
    #[instrument(skip_all)]
    fn validate_request<'a, 'b>(
        &'b self,
        request: &api::PaymentsRequest,
        merchant_account: &'a storage::MerchantAccount,
    ) -> RouterResult<(
        BoxedOperation<'b, F, api::PaymentsRequest>,
        &'a str,
        api::PaymentIdType,
        Option<api::MandateTxnType>,
    )> {
        let given_payment_id = match &request.payment_id {
            Some(id_type) => Some(
                id_type
                    .get_payment_intent_id()
                    .change_context(errors::ApiErrorResponse::PaymentNotFound)?,
            ),
            None => None,
        };

        let request_merchant_id = request.merchant_id.as_deref();
        helpers::validate_merchant_id(&merchant_account.merchant_id, request_merchant_id)
            .change_context(errors::ApiErrorResponse::MerchantAccountNotFound)?;

        let amount = request.amount.get_required_value("amount")?;

        helpers::validate_request_amount_and_amount_to_capture(
            Some(amount),
            request.amount_to_capture,
        )
        .change_context(errors::ApiErrorResponse::InvalidDataFormat {
            field_name: "amount_to_capture".to_string(),
            expected_format: "amount_to_capture lesser than amount".to_string(),
        })?;

        let mandate_type = helpers::validate_mandate(request)?;
        let payment_id = core_utils::get_or_generate_id("payment_id", &given_payment_id, "pay")?;

        Ok((
            Box::new(self),
            &merchant_account.merchant_id,
            api::PaymentIdType::PaymentIntentId(payment_id),
            mandate_type,
        ))
    }
}

impl PaymentCreate {
    #[instrument(skip_all)]
    fn make_payment_attempt(
        payment_id: &str,
        merchant_id: &str,
        connector: types::Connector,
        money: (i32, enums::Currency),
        payment_method: Option<enums::PaymentMethodType>,
        request: &api::PaymentsRequest,
    ) -> storage::PaymentAttemptNew {
        let created_at @ modified_at @ last_synced = Some(crate::utils::date_time::now());
        let status =
            helpers::payment_attempt_status_fsm(&request.payment_method_data, request.confirm);
        let (amount, currency) = (money.0, Some(money.1));
        storage::PaymentAttemptNew {
            payment_id: payment_id.to_string(),
            merchant_id: merchant_id.to_string(),
            txn_id: Uuid::new_v4().to_string(),
            status,
            amount,
            currency,
            connector: connector.to_string(),
            payment_method,
            capture_method: request.capture_method,
            capture_on: request.capture_on,
            confirm: request.confirm.unwrap_or(false),
            created_at,
            modified_at,
            last_synced,
            authentication_type: request.authentication_type,
            ..storage::PaymentAttemptNew::default()
        }
    }

    #[instrument(skip_all)]
    fn make_payment_intent(
        payment_id: &str,
        merchant_id: &str,
        connector_id: &str,
        money: (i32, enums::Currency),
        request: &api::PaymentsRequest,
        shipping_address_id: Option<String>,
        billing_address_id: Option<String>,
    ) -> storage::PaymentIntentNew {
        let created_at @ modified_at @ last_synced = Some(crate::utils::date_time::now());
        let status =
            helpers::payment_intent_status_fsm(&request.payment_method_data, request.confirm);
        let client_secret =
            crate::utils::generate_id(consts::ID_LENGTH, format!("{payment_id}_secret").as_str());
        let (amount, currency) = (money.0, Some(money.1));
        storage::PaymentIntentNew {
            payment_id: payment_id.to_string(),
            merchant_id: merchant_id.to_string(),
            status,
            amount,
            currency,
            connector_id: Some(connector_id.to_string()),
            description: request.description.clone(),
            created_at,
            modified_at,
            last_synced,
            client_secret: Some(client_secret),
            setup_future_usage: request.setup_future_usage,
            off_session: request.off_session,
            return_url: request.return_url.clone(),
            shipping_address_id,
            billing_address_id,
            statement_descriptor_name: request.statement_descriptor_name.clone(),
            statement_descriptor_suffix: request.statement_descriptor_suffix.clone(),
            ..storage::PaymentIntentNew::default()
        }
    }

    #[instrument(skip_all)]
    fn make_connector_response(
        payment_attempt: &storage::PaymentAttempt,
    ) -> storage::ConnectorResponseNew {
        storage::ConnectorResponseNew {
            payment_id: payment_attempt.payment_id.clone(),
            merchant_id: payment_attempt.merchant_id.clone(),
            txn_id: payment_attempt.txn_id.clone(),
            created_at: payment_attempt.created_at,
            modified_at: payment_attempt.modified_at,
            connector_name: payment_attempt.connector.clone(),
            connector_transaction_id: None,
            authentication_data: None,
            encoded_data: None,
        }
    }
}

#[instrument(skip_all)]
pub fn payments_create_request_validation(
    req: &api::PaymentsRequest,
) -> RouterResult<(i32, enums::Currency)> {
    let currency: enums::Currency = req
        .currency
        .as_ref()
        .parse_enum("currency")
        .change_context(errors::ApiErrorResponse::InvalidRequestData {
            message: "invalid currency".to_string(),
        })?;
    let amount = req.amount.get_required_value("amount")?;
    Ok((amount, currency))
}