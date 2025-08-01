use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::borrow::Cow;
use std::path::Path;
use std::sync::Arc;

use crate::config_parser::path::TomlRelativePath;
use crate::config_parser::{LoadableConfig, PathWithContents};
use crate::embeddings::EmbeddingModelTable;
use crate::endpoints::inference::{InferenceClients, InferenceModels, InferenceParams};
use crate::error::{Error, ErrorDetails};
use crate::function::FunctionConfig;
use crate::inference::types::extra_body::{ExtraBodyConfig, FullExtraBodyConfig};
use crate::inference::types::extra_headers::{ExtraHeadersConfig, FullExtraHeadersConfig};
use crate::inference::types::{
    batch::StartBatchModelInferenceWithMetadata, ContentBlock, InferenceResultStream,
    ModelInferenceRequest, RequestMessage, Role,
};
use crate::inference::types::{
    InferenceResult, ModelInput, ResolvedInput, ResolvedInputMessage, ResolvedInputMessageContent,
};
use crate::jsonschema_util::StaticJSONSchema;
use crate::minijinja_util::TemplateConfig;
use crate::model::ModelTable;
use crate::variant::JsonMode;

use super::{
    infer_model_request, infer_model_request_stream, prepare_model_inference_request,
    InferModelRequestArgs, InferenceConfig, ModelUsedInfo, RetryConfig, Variant,
};

#[derive(Debug, Default, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export))]
pub struct ChatCompletionConfig {
    pub weight: Option<f64>,
    pub model: Arc<str>,
    pub system_template: Option<PathWithContents>,
    pub user_template: Option<PathWithContents>,
    pub assistant_template: Option<PathWithContents>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub max_tokens: Option<u32>,
    pub presence_penalty: Option<f32>,
    pub frequency_penalty: Option<f32>,
    pub seed: Option<u32>,
    pub stop_sequences: Option<Vec<String>>,
    pub json_mode: Option<JsonMode>, // Only for JSON functions, not for chat functions
    pub retries: RetryConfig,
    pub extra_body: Option<ExtraBodyConfig>,
    pub extra_headers: Option<ExtraHeadersConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UninitializedChatCompletionConfig {
    #[serde(default)]
    pub weight: Option<f64>,
    pub model: Arc<str>,
    pub system_template: Option<TomlRelativePath>,
    pub user_template: Option<TomlRelativePath>,
    pub assistant_template: Option<TomlRelativePath>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub max_tokens: Option<u32>,
    pub presence_penalty: Option<f32>,
    pub frequency_penalty: Option<f32>,
    pub seed: Option<u32>,
    pub stop_sequences: Option<Vec<String>>,
    #[serde(default)]
    pub json_mode: Option<JsonMode>, // Only for JSON functions, not for chat functions
    #[serde(default)]
    pub retries: RetryConfig,
    #[serde(default)]
    pub extra_body: Option<ExtraBodyConfig>,
    #[serde(default)]
    pub extra_headers: Option<ExtraHeadersConfig>,
}

impl LoadableConfig<ChatCompletionConfig> for UninitializedChatCompletionConfig {
    fn load<P: AsRef<Path>>(self, base_path: P) -> Result<ChatCompletionConfig, Error> {
        Ok(ChatCompletionConfig {
            weight: self.weight,
            model: self.model,
            system_template: self
                .system_template
                .map(|path| PathWithContents::from_path(path, Some(&base_path)))
                .transpose()?,
            user_template: self
                .user_template
                .map(|path| PathWithContents::from_path(path, Some(&base_path)))
                .transpose()?,
            assistant_template: self
                .assistant_template
                .map(|path| PathWithContents::from_path(path, Some(&base_path)))
                .transpose()?,
            temperature: self.temperature,
            top_p: self.top_p,
            max_tokens: self.max_tokens,
            presence_penalty: self.presence_penalty,
            frequency_penalty: self.frequency_penalty,
            seed: self.seed,
            stop_sequences: self.stop_sequences,
            json_mode: self.json_mode,
            retries: self.retries,
            extra_body: self.extra_body,
            extra_headers: self.extra_headers,
        })
    }
}

impl ChatCompletionConfig {
    pub fn prepare_request_message(
        &self,
        templates: &TemplateConfig,
        message: &ResolvedInputMessage,
        template_schema_info: TemplateSchemaInfo,
    ) -> Result<RequestMessage, Error> {
        let template_path = match message.role {
            Role::User => self.user_template.as_ref(),
            Role::Assistant => self.assistant_template.as_ref(),
        }
        .map(|x| {
            x.path
                .path()
                .to_str()
                .ok_or_else(|| Error::new(ErrorDetails::InvalidTemplatePath))
        })
        .transpose()?;
        prepare_request_message(message, templates, template_path, template_schema_info)
    }

    pub fn prepare_system_message(
        &self,
        templates: &TemplateConfig,
        system: Option<&Value>,
        template_schema_info: TemplateSchemaInfo,
    ) -> Result<Option<String>, Error> {
        let template_path = self
            .system_template
            .as_ref()
            .map(|x| {
                x.path
                    .path()
                    .to_str()
                    .ok_or_else(|| Error::new(ErrorDetails::InvalidTemplatePath))
            })
            .transpose()?;
        prepare_system_message(system, templates, template_path, template_schema_info)
    }

    fn prepare_request<'a, 'request>(
        &'a self,
        input: &ResolvedInput,
        function: &'a FunctionConfig,
        inference_config: &'request InferenceConfig<'a, 'request>,
        stream: bool,
        inference_params: &mut InferenceParams,
    ) -> Result<ModelInferenceRequest<'request>, Error> {
        let messages = input
            .messages
            .iter()
            .map(|message| {
                self.prepare_request_message(
                    inference_config.templates,
                    message,
                    function.template_schema_info(),
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let system = self.prepare_system_message(
            inference_config.templates,
            input.system.as_ref(),
            function.template_schema_info(),
        )?;

        inference_params
            .chat_completion
            .backfill_with_variant_params(
                self.temperature,
                self.max_tokens,
                self.seed,
                self.top_p,
                self.presence_penalty,
                self.frequency_penalty,
                self.stop_sequences.clone(),
            );

        let extra_body = FullExtraBodyConfig {
            extra_body: self.extra_body.clone(),
            inference_extra_body: inference_config
                .extra_body
                .clone()
                .into_owned()
                .filter(inference_config.variant_name),
        };

        let extra_headers = FullExtraHeadersConfig {
            variant_extra_headers: self.extra_headers.clone(),
            inference_extra_headers: inference_config
                .extra_headers
                .clone()
                .into_owned()
                .filter(inference_config.variant_name),
        };

        prepare_model_inference_request(
            messages,
            system,
            function,
            inference_config,
            stream,
            inference_params,
            self.json_mode,
            extra_body,
            extra_headers,
        )
    }
}

/// Prepare a ModelInput using the same machinery as is used by core TensorZero to prepare
/// chat completions requests.
pub fn prepare_model_input(
    system: Option<&Value>,
    messages: &[ResolvedInputMessage],
    templates: &TemplateConfig,
    system_template_name: Option<&str>,
    user_template_name: Option<&str>,
    assistant_template_name: Option<&str>,
    template_schema_info: TemplateSchemaInfo,
) -> Result<ModelInput, Error> {
    let system = prepare_system_message(
        system,
        templates,
        system_template_name,
        template_schema_info,
    )?;
    let mut templated_messages = Vec::with_capacity(messages.len());
    for message in messages.iter() {
        let template_name = match message.role {
            Role::Assistant => assistant_template_name,
            Role::User => user_template_name,
        };
        templated_messages.push(prepare_request_message(
            message,
            templates,
            template_name,
            template_schema_info,
        )?);
    }
    Ok(ModelInput {
        system,
        messages: templated_messages,
    })
}

fn prepare_system_message(
    system: Option<&Value>,
    templates: &TemplateConfig,
    template_name: Option<&str>,
    template_schema_info: TemplateSchemaInfo,
) -> Result<Option<String>, Error> {
    Ok(match template_name {
        Some(template_path) => {
            let context = if template_schema_info.has_system_schema {
                Cow::Borrowed(system.unwrap_or(&Value::Null))
            } else {
                Cow::Owned(serde_json::json!({
                    SYSTEM_TEXT_TEMPLATE_VAR: system.unwrap_or(&Value::Null)
                }))
            };
            Some(templates.template_message(
            template_path,
            &context)?)
        }
        None => {
            match system {
                None => None,
                Some(system) =>
            Some(system
            .as_str()
            .ok_or_else(|| Error::new(ErrorDetails::InvalidMessage {
                message:
                    format!("System message content {system} is not a string but there is no variant template")
                        .to_string(),
            }))?
            .to_string()),
        }
    }})
}

fn prepare_request_message(
    message: &ResolvedInputMessage,
    templates: &TemplateConfig,
    template_name: Option<&str>,
    template_schema_info: TemplateSchemaInfo,
) -> Result<RequestMessage, Error> {
    let mut content = Vec::new();
    // If a schema is provided, then we'll just use the `ResolvedInputMessageContent::Text`
    // json value as-is when applying the template.
    // If a schema is not provided, then we create a single variable (based on the template kind),
    // and set it to the string contents of the 'ResolvedInputMessageContent::Text
    let template_var = match message.role {
        Role::User => {
            if template_schema_info.has_user_schema {
                None
            } else {
                Some(USER_TEXT_TEMPLATE_VAR)
            }
        }
        Role::Assistant => {
            if template_schema_info.has_assistant_schema {
                None
            } else {
                Some(ASSISTANT_TEXT_TEMPLATE_VAR)
            }
        }
    };
    for block in message.content.iter() {
        match block {
            ResolvedInputMessageContent::Text { value } => {
                let text_content= match template_name {
                    Some(template_path) => {
                        let context =if let Some(template_var) = template_var {
                            let message_text = value.as_str().ok_or_else(|| {
                                Error::new(ErrorDetails::InvalidMessage { message: format!("Request message content {} is not a string but template (without schema) is provided for Role {}", value, message.role) })
                            })?;
                            Cow::Owned(serde_json::json!({
                                template_var: message_text
                            }))
                        } else {
                            Cow::Borrowed(value)
                        };
                        templates.template_message(
                        template_path,
                        &context)?
                    }
                    None => value.as_str().ok_or_else(|| {
                        Error::new(ErrorDetails::InvalidMessage { message: format!("Request message content {} is not a string but there is no variant template for Role {}", value, message.role) })
                    })?.to_string()
                };
                content.push(text_content.into());
            }
            ResolvedInputMessageContent::RawText { value: text } => {
                content.push(text.clone().into());
            }
            // The following two clones are probably removable.
            // We will need to implement a ToolCallRef type or something so that we can avoid cloning the ToolCall and ToolResult.
            ResolvedInputMessageContent::ToolCall(tool_call) => {
                content.push(ContentBlock::ToolCall(tool_call.clone()));
            }
            ResolvedInputMessageContent::ToolResult(tool_result) => {
                content.push(ContentBlock::ToolResult(tool_result.clone()));
            }
            ResolvedInputMessageContent::File(image) => {
                content.push(ContentBlock::File(image.clone()));
            }
            ResolvedInputMessageContent::Thought(thought) => {
                content.push(ContentBlock::Thought(thought.clone()));
            }
            ResolvedInputMessageContent::Unknown {
                data,
                model_provider_name,
            } => {
                content.push(ContentBlock::Unknown {
                    data: data.clone(),
                    model_provider_name: model_provider_name.clone(),
                });
            }
        }
    }

    Ok(RequestMessage {
        role: message.role,
        content,
    })
}

impl Variant for ChatCompletionConfig {
    async fn infer<'a: 'request, 'request>(
        &self,
        input: &ResolvedInput,
        models: &'request InferenceModels<'a>,
        function: &'a FunctionConfig,
        inference_config: &'request InferenceConfig<'static, 'request>,
        clients: &'request InferenceClients<'request>,
        inference_params: InferenceParams,
    ) -> Result<InferenceResult, Error> {
        let mut inference_params = inference_params;
        let request = self.prepare_request(
            input,
            function,
            inference_config,
            false,
            &mut inference_params,
        )?;
        let model_config = models.models.get(&self.model).await?.ok_or_else(|| {
            Error::new(ErrorDetails::UnknownModel {
                name: self.model.to_string(),
            })
        })?;
        let args = InferModelRequestArgs {
            request,
            model_name: self.model.clone(),
            model_config: &model_config,
            function,
            inference_config,
            clients,
            inference_params,
            retry_config: &self.retries,
        };
        infer_model_request(args).await
    }

    async fn infer_stream<'request>(
        &self,
        input: &ResolvedInput,
        models: &'request InferenceModels<'_>,
        function: &FunctionConfig,
        inference_config: &'request InferenceConfig<'static, 'request>,
        clients: &'request InferenceClients<'request>,
        inference_params: InferenceParams,
    ) -> Result<(InferenceResultStream, ModelUsedInfo), Error> {
        let mut inference_params = inference_params;
        let request = self.prepare_request(
            input,
            function,
            inference_config,
            true,
            &mut inference_params,
        )?;
        let model_config = models.models.get(&self.model).await?.ok_or_else(|| {
            Error::new(ErrorDetails::UnknownModel {
                name: self.model.to_string(),
            })
        })?;
        infer_model_request_stream(
            request,
            self.model.clone(),
            &model_config,
            function,
            clients,
            inference_params,
            self.retries,
        )
        .await
    }

    /// This function validates that the chat completion variant is correctly configured for the given function config.
    /// In order to do this, we need to check:
    ///  - For system, user, and assistant message types:
    ///    - That the template here is provided if the schema is provided in the function.
    ///    - If the template requires variables, the schema is provided.
    ///  - That the model name is a valid model
    ///  - That the weight is non-negative
    async fn validate(
        &self,
        function: &FunctionConfig,
        models: &mut ModelTable,
        _embedding_models: &EmbeddingModelTable,
        templates: &TemplateConfig<'_>,
        function_name: &str,
        variant_name: &str,
    ) -> Result<(), Error> {
        // Validate that weight is non-negative
        if self.weight.is_some_and(|w| w < 0.0) {
            return Err(ErrorDetails::Config {
                message: format!(
                    "`functions.{function_name}.variants.{variant_name}`: `weight` must be non-negative"
                ),
            }.into());
        }
        models.validate(&self.model)?;

        // Validate the system template matches the system schema (best effort, we cannot check the variables comprehensively)
        validate_template_and_schema(
            TemplateKind::System,
            function.system_schema(),
            self.system_template.as_ref().map(|t| &t.path),
            templates,
        )
        .map_err(|e| {
            Error::new(ErrorDetails::Config {
                message: format!(
                    "`functions.{function_name}.variants.{variant_name}.system_template`: {e}"
                ),
            })
        })?;

        // Validate the user template matches the user schema (best effort, we cannot check the variables comprehensively)
        validate_template_and_schema(
            TemplateKind::User,
            function.user_schema(),
            self.user_template.as_ref().map(|t| &t.path),
            templates,
        )
        .map_err(|e| {
            Error::new(ErrorDetails::Config {
                message: format!(
                    "`functions.{function_name}.variants.{variant_name}.user_template`: {e}"
                ),
            })
        })?;

        // Validate the assistant template matches the assistant schema (best effort, we cannot check the variables comprehensively)
        validate_template_and_schema(
            TemplateKind::Assistant,
            function.assistant_schema(),
            self.assistant_template.as_ref().map(|t| &t.path),
            templates,
        )
        .map_err(|e| {
            Error::new(ErrorDetails::Config {
                message: format!(
                    "`functions.{function_name}.variants.{variant_name}.assistant_template`: {e}"
                ),
            })
        })?;
        Ok(())
    }

    fn get_all_template_paths(&self) -> Vec<&PathWithContents> {
        let mut templates = Vec::new();
        if let Some(system_template) = &self.system_template {
            templates.push(system_template);
        }
        if let Some(user_template) = &self.user_template {
            templates.push(user_template);
        }
        if let Some(assistant_template) = &self.assistant_template {
            templates.push(assistant_template);
        }
        templates
    }

    async fn start_batch_inference<'a>(
        &'a self,
        inputs: &[ResolvedInput],
        models: &'a InferenceModels<'a>,
        function: &'a FunctionConfig,
        inference_configs: &'a [InferenceConfig<'a, 'a>],
        clients: &'a InferenceClients<'a>,
        inference_params: Vec<InferenceParams>,
    ) -> Result<StartBatchModelInferenceWithMetadata<'a>, Error> {
        // First construct all inference configs so they stick around for the duration of this function body
        let mut inference_params = inference_params;

        // Next, prepare all the ModelInferenceRequests
        let mut inference_requests = Vec::new();
        for ((input, inference_param), inference_config) in inputs
            .iter()
            .zip(&mut inference_params)
            .zip(inference_configs)
        {
            let request =
                self.prepare_request(input, function, inference_config, false, inference_param)?;
            inference_requests.push(request);
        }
        let model_config = models.models.get(&self.model).await?.ok_or_else(|| {
            Error::new(ErrorDetails::UnknownModel {
                name: self.model.to_string(),
            })
        })?;
        let model_inference_response = model_config
            .start_batch_inference(
                &inference_requests,
                clients.http_client,
                clients.credentials,
            )
            .await?;
        Ok(StartBatchModelInferenceWithMetadata::new(
            model_inference_response,
            inference_requests,
            &self.model,
            inference_params,
        ))
    }
}

/// Used to determine how to apply templates to input content blocks
/// If we have a schema, then we forward the 'arguments' object as-is to the template.
/// If we don't have a schema, then we create a single variable corresponding to the template
/// kind (e.g. `SYSTEM_TEXT_TEMPLATE_VAR` for a system template), and set this variable the
/// string contents of the input block.
#[derive(Copy, Clone, Debug)]
pub struct TemplateSchemaInfo {
    pub has_system_schema: bool,
    pub has_user_schema: bool,
    pub has_assistant_schema: bool,
}

/// The template variable names used when applying a template with no schema
/// Only one of these variables is used per template, based on the `TemplateKind`
const SYSTEM_TEXT_TEMPLATE_VAR: &str = "system_text";
const USER_TEXT_TEMPLATE_VAR: &str = "user_text";
const ASSISTANT_TEXT_TEMPLATE_VAR: &str = "assistant_text";

#[derive(Copy, Clone, Debug)]
pub enum TemplateKind {
    System,
    User,
    Assistant,
}

pub fn validate_template_and_schema(
    kind: TemplateKind,
    schema: Option<&StaticJSONSchema>,
    template: Option<&TomlRelativePath>,
    templates: &TemplateConfig,
) -> Result<(), Error> {
    match (schema, template) {
        (None, Some(template)) => {
            let template_name = template
                .path()
                .to_str()
                .ok_or_else(|| Error::new(ErrorDetails::InvalidTemplatePath))?;
            let undeclared_vars = templates.get_undeclared_variables(template_name)?;
            let allowed_var = match kind {
                TemplateKind::System => SYSTEM_TEXT_TEMPLATE_VAR,
                TemplateKind::User => USER_TEXT_TEMPLATE_VAR,
                TemplateKind::Assistant => ASSISTANT_TEXT_TEMPLATE_VAR,
            };
            // When we have no schema, the template can have at most one variable
            if !undeclared_vars.is_empty() {
                // If the template has any variables, it must be the one allowed variable (e.g. `system_text`)
                // based on the template kind
                let mut undeclared_vars = undeclared_vars.into_iter().collect::<Vec<_>>();
                if undeclared_vars != [allowed_var.to_string()] {
                    // Ensure that the error message is deterministic
                    undeclared_vars.sort();
                    let undeclared_vars_str = format!("[{}]", undeclared_vars.join(", "));
                    return Err(Error::new(ErrorDetails::Config {
                        message:
                            format!("template needs variables: {undeclared_vars_str} but only `{allowed_var}` is allowed when template has no schema")
                                .to_string(),
                    }));
                }
            }
        }
        (Some(_), None) => {
            return Err(Error::new(ErrorDetails::Config {
                message: "template is required when schema is specified".to_string(),
            }));
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use super::*;

    use futures::StreamExt;
    use reqwest::Client;
    use serde_json::{json, Value};
    use uuid::Uuid;

    use crate::cache::{CacheEnabledMode, CacheOptions};
    use crate::clickhouse::ClickHouseConnectionInfo;
    use crate::embeddings::EmbeddingModelTable;
    use crate::endpoints::inference::{
        ChatCompletionInferenceParams, InferenceCredentials, InferenceIds,
    };
    use crate::function::{FunctionConfigChat, FunctionConfigJson};
    use crate::inference::types::{
        ContentBlockChatOutput, InferenceResultChunk, ModelInferenceRequestJsonMode, Usage,
    };
    use crate::jsonschema_util::{DynamicJSONSchema, StaticJSONSchema};
    use crate::minijinja_util::tests::get_test_template_config;
    use crate::model::{ModelConfig, ModelProvider, ProviderConfig};
    use crate::providers::dummy::{DummyProvider, DUMMY_JSON_RESPONSE_RAW};
    use crate::providers::test_helpers::get_temperature_tool_config;
    use crate::tool::{ToolCallConfig, ToolChoice};
    use crate::{
        error::Error,
        inference::types::{ContentBlockChunk, Role, TextChunk},
        providers::dummy::{DUMMY_INFER_RESPONSE_CONTENT, DUMMY_STREAMING_RESPONSE},
    };

    #[test]
    fn test_prepare_request_message() {
        let templates = get_test_template_config();
        // Part 1: test without templates
        let chat_completion_config = ChatCompletionConfig {
            model: "dummy".into(),
            weight: Some(1.0),
            system_template: None,
            user_template: None,
            assistant_template: None,
            json_mode: Some(JsonMode::On),
            temperature: None,
            top_p: None,
            presence_penalty: None,
            frequency_penalty: None,
            max_tokens: None,
            seed: None,
            stop_sequences: None,
            retries: RetryConfig::default(),
            extra_body: Default::default(),
            extra_headers: Default::default(),
        };

        let all_schemas = TemplateSchemaInfo {
            has_system_schema: true,
            has_user_schema: true,
            has_assistant_schema: true,
        };

        // Test case 1: Regular user message
        let input_message = ResolvedInputMessage {
            role: Role::User,
            content: vec!["Hello, how are you?".to_string().into()],
        };
        let result =
            chat_completion_config.prepare_request_message(&templates, &input_message, all_schemas);
        assert!(result.is_ok());
        let prepared_message = result.unwrap();
        match prepared_message {
            RequestMessage {
                role: Role::User,
                content: user_message,
            } => {
                assert_eq!(user_message, vec!["Hello, how are you?".to_string().into()]);
            }
            _ => panic!("Expected User message"),
        }

        // Test case 2: Assistant message
        let input_message = ResolvedInputMessage {
            role: Role::Assistant,
            content: vec!["I'm doing well, thank you!".to_string().into()],
        };
        let result =
            chat_completion_config.prepare_request_message(&templates, &input_message, all_schemas);
        assert!(result.is_ok());
        let prepared_message = result.unwrap();
        match prepared_message {
            RequestMessage {
                role: Role::Assistant,
                content: assistant_message,
            } => {
                assert_eq!(
                    assistant_message,
                    vec!["I'm doing well, thank you!".to_string().into()]
                );
            }
            _ => panic!("Expected Assistant message"),
        }
        // Test case 3: Invalid JSON input
        let input_message = ResolvedInputMessage {
            role: Role::User,
            content: vec![json!({"invalid": "json"}).into()],
        };
        let result = chat_completion_config
            .prepare_request_message(&templates, &input_message, all_schemas)
            .unwrap_err();
        assert_eq!(result, ErrorDetails::InvalidMessage { message: "Request message content {\"invalid\":\"json\"} is not a string but there is no variant template for Role user".to_string()}.into());

        // Part 2: test with templates
        let system_template_name = "system";
        let user_template_name = "greeting_with_age";
        let assistant_template_name = "assistant";

        let chat_completion_config = ChatCompletionConfig {
            model: "dummy".into(),
            weight: Some(1.0),
            system_template: Some(PathWithContents {
                path: system_template_name.into(),
                contents: String::new(),
            }),
            user_template: Some(PathWithContents {
                path: user_template_name.into(),
                contents: String::new(),
            }),
            assistant_template: Some(PathWithContents {
                path: assistant_template_name.into(),
                contents: String::new(),
            }),
            json_mode: Some(JsonMode::On),
            ..Default::default()
        };

        // Test case 4: Assistant message with template
        let input_message = ResolvedInputMessage {
            role: Role::Assistant,
            content: vec![json!({"reason": "it's against my ethical guidelines"}).into()],
        };
        let result =
            chat_completion_config.prepare_request_message(&templates, &input_message, all_schemas);
        assert!(result.is_ok());
        let prepared_message = result.unwrap();
        match prepared_message {
            RequestMessage {
                role: Role::Assistant,
                content: assistant_message,
            } => {
                assert_eq!(
                    assistant_message,
                    vec!["I'm sorry but I can't help you with that because of it's against my ethical guidelines".to_string().into()]
                );
            }
            _ => panic!("Expected Assistant message"),
        }

        // Test case 5: User message with template
        let input_message = ResolvedInputMessage {
            role: Role::User,
            content: vec![json!({"name": "John", "age": 30}).into()],
        };
        let result =
            chat_completion_config.prepare_request_message(&templates, &input_message, all_schemas);
        assert!(result.is_ok());
        let prepared_message = result.unwrap();
        match prepared_message {
            RequestMessage {
                role: Role::User,
                content: user_message,
            } => {
                assert_eq!(
                    user_message,
                    vec!["Hello, John! You are 30 years old.".to_string().into()]
                );
            }
            _ => panic!("Expected User message"),
        }

        // Test case 6: User message with bad input (missing required field)
        let input_message = ResolvedInputMessage {
            role: Role::User,
            content: vec![json!({"name": "Alice"}).into()], // Missing "age" field
        };
        let result =
            chat_completion_config.prepare_request_message(&templates, &input_message, all_schemas);
        assert!(result.is_err());
        match result.unwrap_err().get_details() {
            ErrorDetails::MiniJinjaTemplateRender { message, .. } => {
                assert!(message.contains("undefined value"));
            }
            _ => panic!("Expected MiniJinjaTemplateRender error"),
        }
        // Test case 7: User message with string content when template is provided
        let input_message = ResolvedInputMessage {
            role: Role::User,
            content: vec!["This is a plain string".to_string().into()],
        };
        let result =
            chat_completion_config.prepare_request_message(&templates, &input_message, all_schemas);
        assert!(result.is_err());
        match result.unwrap_err().get_details() {
            ErrorDetails::MiniJinjaTemplateRender { message, .. } => {
                assert!(message.contains("undefined value"), "{}", message);
            }
            _ => panic!("Expected MiniJinjaTemplateRender error"),
        }
        // Part 3: test with filled out templates
        let system_template_name = "system";
        let user_template_name = "user_filled";
        let assistant_template_name = "assistant_filled";

        let chat_completion_config = ChatCompletionConfig {
            model: "dummy".into(),
            weight: Some(1.0),
            system_template: Some(PathWithContents {
                path: system_template_name.into(),
                contents: String::new(),
            }),
            user_template: Some(PathWithContents {
                path: user_template_name.into(),
                contents: String::new(),
            }),
            assistant_template: Some(PathWithContents {
                path: assistant_template_name.into(),
                contents: String::new(),
            }),
            json_mode: Some(JsonMode::On),
            ..Default::default()
        };

        // Test case 8: assistant message with null input and filled out template
        let input_message = ResolvedInputMessage {
            role: Role::Assistant,
            content: vec![Value::Null.into()],
        };
        let result =
            chat_completion_config.prepare_request_message(&templates, &input_message, all_schemas);
        assert!(result.is_ok());
        let prepared_message = result.unwrap();
        match prepared_message {
            RequestMessage {
                role: Role::Assistant,
                content: assistant_message,
            } => {
                assert_eq!(
                    assistant_message,
                    vec!["I'm sorry but I can't help you with that because of it's against my ethical guidelines".to_string().into()]
                );
            }
            _ => panic!("Expected Assistant message"),
        }

        // Test case 9: User message with null input and filled out template
        let input_message = ResolvedInputMessage {
            role: Role::User,
            content: vec![Value::Null.into()],
        };
        let result =
            chat_completion_config.prepare_request_message(&templates, &input_message, all_schemas);
        assert!(result.is_ok());
        let prepared_message = result.unwrap();
        match prepared_message {
            RequestMessage {
                role: Role::User,
                content: user_message,
            } => {
                assert_eq!(
                    user_message,
                    vec!["What's the capital of Japan?".to_string().into()]
                );
            }
            _ => panic!("Expected User message"),
        }
    }

    #[test]
    fn test_prepare_system_message() {
        let templates = get_test_template_config();

        let all_schemas = TemplateSchemaInfo {
            has_system_schema: true,
            has_user_schema: true,
            has_assistant_schema: true,
        };

        // Test without templates, string message
        let chat_completion_config = ChatCompletionConfig {
            model: "dummy".into(),
            weight: Some(1.0),
            ..Default::default()
        };
        let input_message = Value::String("You are a helpful assistant.".to_string());
        let result = chat_completion_config.prepare_system_message(
            &templates,
            Some(&input_message),
            all_schemas,
        );
        assert!(result.is_ok());
        let prepared_message = result.unwrap();
        assert_eq!(
            prepared_message,
            Some("You are a helpful assistant.".to_string())
        );

        // Test without templates, object message
        let chat_completion_config = ChatCompletionConfig {
            model: "dummy".into(),
            weight: Some(1.0),
            ..Default::default()
        };
        let input_message = json!({"message": "You are a helpful assistant."});
        let result = chat_completion_config.prepare_system_message(
            &templates,
            Some(&input_message),
            all_schemas,
        );
        assert!(result.is_err());
        let prepared_message = result.unwrap_err();
        assert_eq!(
            prepared_message,
            ErrorDetails::InvalidMessage { message: "System message content {\"message\":\"You are a helpful assistant.\"} is not a string but there is no variant template".to_string() }.into()
        );

        // Test without templates, no message
        let chat_completion_config = ChatCompletionConfig {
            model: "dummy".into(),
            weight: Some(1.0),
            ..Default::default()
        };
        let result = chat_completion_config.prepare_system_message(&templates, None, all_schemas);
        assert!(result.is_ok());
        let prepared_message = result.unwrap();
        assert_eq!(prepared_message, None);

        // Test with templates that need new info
        let system_template_name = "system";

        let chat_completion_config = ChatCompletionConfig {
            model: "dummy".into(),
            weight: Some(1.0),
            system_template: Some(PathWithContents {
                path: system_template_name.into(),
                contents: String::new(),
            }),
            ..Default::default()
        };

        let input_message = serde_json::json!({"assistant_name": "ChatGPT"});
        let result = chat_completion_config.prepare_system_message(
            &templates,
            Some(&input_message),
            all_schemas,
        );
        assert!(result.is_ok());
        let prepared_message = result.unwrap();
        assert_eq!(
            prepared_message,
            Some("You are a helpful and friendly assistant named ChatGPT".to_string())
        );

        // Test with template that is complete as is (string)
        let system_template_name = "system_filled";

        let chat_completion_config = ChatCompletionConfig {
            model: "dummy".into(),
            weight: Some(1.0),
            system_template: Some(PathWithContents {
                path: system_template_name.into(),
                contents: String::new(),
            }),
            ..Default::default()
        };

        let result = chat_completion_config.prepare_system_message(&templates, None, all_schemas);
        assert!(result.is_ok());
        let prepared_message = result.unwrap();
        assert_eq!(
            prepared_message,
            Some("You are a helpful and friendly assistant named ChatGPT".to_string())
        );
    }

    #[tokio::test]
    async fn test_infer_chat_completion() {
        let client = Client::new();
        let clickhouse_connection_info = ClickHouseConnectionInfo::Disabled;
        let api_keys = InferenceCredentials::default();
        let clients = InferenceClients {
            http_client: &client,
            clickhouse_connection_info: &clickhouse_connection_info,
            credentials: &api_keys,
            cache_options: &CacheOptions {
                max_age_s: None,
                enabled: CacheEnabledMode::WriteOnly,
            },
        };
        let templates = get_test_template_config();
        let system_template_name = "system";
        let user_template_name = "greeting_with_age";
        let chat_completion_config = ChatCompletionConfig {
            model: "good".into(),
            weight: Some(1.0),
            system_template: Some(PathWithContents {
                path: system_template_name.into(),
                contents: String::new(),
            }),
            user_template: Some(PathWithContents {
                path: user_template_name.into(),
                contents: String::new(),
            }),
            ..Default::default()
        };
        let schema_any = StaticJSONSchema::from_value(&json!({ "type": "object" })).unwrap();
        let function_config = FunctionConfig::Chat(FunctionConfigChat {
            variants: HashMap::new(),
            system_schema: Some(schema_any.clone()),
            user_schema: Some(schema_any.clone()),
            assistant_schema: Some(schema_any.clone()),
            tools: vec![],
            tool_choice: ToolChoice::Auto,
            parallel_tool_calls: None,
            description: None,
        });
        let good_provider_config = ProviderConfig::Dummy(DummyProvider {
            model_name: "good".into(),
            ..Default::default()
        });
        let error_provider_config = ProviderConfig::Dummy(DummyProvider {
            model_name: "error".into(),
            ..Default::default()
        });
        let json_provider_config = ProviderConfig::Dummy(DummyProvider {
            model_name: "json".into(),
            ..Default::default()
        });
        let text_model_config = ModelConfig {
            routing: vec!["good".into()],
            providers: HashMap::from([(
                "good".into(),
                ModelProvider {
                    name: "good".into(),
                    config: good_provider_config,
                    extra_body: Default::default(),
                    extra_headers: Default::default(),
                    timeouts: Default::default(),
                    discard_unknown_chunks: false,
                },
            )]),
            timeouts: Default::default(),
        };
        let json_model_config = ModelConfig {
            routing: vec!["json_provider".into()],
            providers: HashMap::from([(
                "json_provider".into(),
                ModelProvider {
                    name: "json_provider".into(),
                    config: json_provider_config,
                    extra_body: Default::default(),
                    extra_headers: Default::default(),
                    timeouts: Default::default(),
                    discard_unknown_chunks: false,
                },
            )]),
            timeouts: Default::default(),
        };
        let tool_provider_config = ProviderConfig::Dummy(DummyProvider {
            model_name: "tool".into(),
            ..Default::default()
        });
        let tool_model_config = ModelConfig {
            routing: vec!["tool_provider".into()],
            providers: HashMap::from([(
                "tool_provider".into(),
                ModelProvider {
                    name: "tool_provider".into(),
                    config: tool_provider_config,
                    extra_body: Default::default(),
                    extra_headers: Default::default(),
                    timeouts: Default::default(),
                    discard_unknown_chunks: false,
                },
            )]),
            timeouts: Default::default(),
        };
        let error_model_config = ModelConfig {
            routing: vec!["error".into()],
            providers: HashMap::from([(
                "error".into(),
                ModelProvider {
                    name: "error".into(),
                    config: error_provider_config,
                    extra_body: Default::default(),
                    extra_headers: Default::default(),
                    timeouts: Default::default(),
                    discard_unknown_chunks: false,
                },
            )]),
            timeouts: Default::default(),
        };
        // Test case 1: invalid message (String passed when template required)
        let messages = vec![ResolvedInputMessage {
            role: Role::User,
            content: vec!["Hello".to_string().into()],
        }];
        let input = ResolvedInput {
            system: Some(Value::String("Hello".to_string())),
            messages,
        };
        let inference_params = InferenceParams::default();
        let inference_config = InferenceConfig {
            templates: &templates,
            tool_config: None,
            function_name: "",
            variant_name: "",
            dynamic_output_schema: None,
            ids: InferenceIds {
                inference_id: Uuid::now_v7(),
                episode_id: Uuid::now_v7(),
            },
            extra_body: Default::default(),
            extra_headers: Default::default(),
            extra_cache_key: None,
        };
        let models = ModelTable::default();
        let inference_models = InferenceModels {
            models: &models,
            embedding_models: &EmbeddingModelTable::default(),
        };
        let result = chat_completion_config
            .infer(
                &input,
                &inference_models,
                &function_config,
                &inference_config,
                &clients,
                inference_params,
            )
            .await
            .unwrap_err();
        match result.get_details() {
            ErrorDetails::MiniJinjaTemplateRender { message, .. } => {
                // template_name is a test filename
                assert!(message.contains("undefined value"));
            }
            _ => panic!("Expected MiniJinjaTemplateRender error"),
        }

        // Test case 2: invalid model in request
        let inference_params = InferenceParams::default();
        let messages = vec![ResolvedInputMessage {
            role: Role::User,
            content: vec![json!({"name": "Luke", "age": 20}).into()],
        }];
        let input = ResolvedInput {
            system: Some(json!({"assistant_name": "R2-D2"})),
            messages,
        };
        let models = HashMap::from([("invalid_model".into(), text_model_config)])
            .try_into()
            .unwrap();
        let inference_models = InferenceModels {
            models: &models,
            embedding_models: &EmbeddingModelTable::default(),
        };
        let inference_config = InferenceConfig {
            templates: &templates,
            tool_config: None,
            function_name: "",
            variant_name: "",
            dynamic_output_schema: None,
            ids: InferenceIds {
                inference_id: Uuid::now_v7(),
                episode_id: Uuid::now_v7(),
            },
            extra_body: Default::default(),
            extra_headers: Default::default(),
            extra_cache_key: None,
        };
        let result = chat_completion_config
            .infer(
                &input,
                &inference_models,
                &function_config,
                &inference_config,
                &clients,
                inference_params,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(result.get_details(), ErrorDetails::UnknownModel { .. }),
            "{}",
            result
        );
        // Test case 3: Model inference fails because of model issues

        let chat_completion_config = ChatCompletionConfig {
            model: "error".into(),
            weight: Some(1.0),
            system_template: Some(PathWithContents {
                path: system_template_name.into(),
                contents: String::new(),
            }),
            user_template: Some(PathWithContents {
                path: user_template_name.into(),
                contents: String::new(),
            }),
            ..Default::default()
        };
        let inference_params = InferenceParams::default();
        let models = HashMap::from([("error".into(), error_model_config)]);
        let models = models.try_into().unwrap();
        let inference_models = InferenceModels {
            models: &models,
            embedding_models: &EmbeddingModelTable::try_from(HashMap::new()).unwrap(),
        };
        let inference_config = InferenceConfig {
            templates: &templates,
            tool_config: None,
            function_name: "",
            variant_name: "",
            dynamic_output_schema: None,
            ids: InferenceIds {
                inference_id: Uuid::now_v7(),
                episode_id: Uuid::now_v7(),
            },
            extra_body: Default::default(),
            extra_headers: Default::default(),
            extra_cache_key: None,
        };
        let err = chat_completion_config
            .infer(
                &input,
                &inference_models,
                &function_config,
                &inference_config,
                &clients,
                inference_params,
            )
            .await
            .unwrap_err();
        let details = err.get_details();
        assert_eq!(
            *details,
            ErrorDetails::ModelProvidersExhausted {
                provider_errors: HashMap::from([(
                    "error".to_string(),
                    Error::new(ErrorDetails::InferenceClient {
                        message: "Error sending request to Dummy provider for model 'error'."
                            .to_string(),
                        status_code: None,
                        raw_request: Some("raw request".to_string()),
                        raw_response: None,
                        provider_type: "dummy".to_string(),
                    })
                )])
            }
        );

        // Test case 4: Model inference succeeds
        let inference_params = InferenceParams::default();
        let chat_completion_config = ChatCompletionConfig {
            model: "good".into(),
            weight: Some(1.0),
            system_template: Some(PathWithContents {
                path: system_template_name.into(),
                contents: String::new(),
            }),
            user_template: Some(PathWithContents {
                path: user_template_name.into(),
                contents: String::new(),
            }),
            ..Default::default()
        };
        let good_provider_config = ProviderConfig::Dummy(DummyProvider {
            model_name: "good".into(),
            ..Default::default()
        });
        let text_model_config = ModelConfig {
            routing: vec!["good_provider".into()],
            providers: HashMap::from([(
                "good_provider".into(),
                ModelProvider {
                    name: "good_provider".into(),
                    config: good_provider_config,
                    extra_body: Default::default(),
                    extra_headers: Default::default(),
                    timeouts: Default::default(),
                    discard_unknown_chunks: false,
                },
            )]),
            timeouts: Default::default(),
        };
        let models = HashMap::from([("good".into(), text_model_config)])
            .try_into()
            .unwrap();
        let inference_models = InferenceModels {
            models: &models,
            embedding_models: &EmbeddingModelTable::default(),
        };
        let inference_config = InferenceConfig {
            templates: &templates,
            tool_config: None,
            function_name: "",
            variant_name: "",
            dynamic_output_schema: None,
            ids: InferenceIds {
                inference_id: Uuid::now_v7(),
                episode_id: Uuid::now_v7(),
            },
            extra_body: Default::default(),
            extra_headers: Default::default(),
            extra_cache_key: None,
        };
        let result = chat_completion_config
            .infer(
                &input,
                &inference_models,
                &function_config,
                &inference_config,
                &clients,
                inference_params.clone(),
            )
            .await
            .unwrap();
        assert!(matches!(result, InferenceResult::Chat(_)));
        assert_eq!(
            result.usage_considering_cached(),
            Usage {
                input_tokens: 10,
                output_tokens: 1,
            }
        );
        match result {
            InferenceResult::Chat(chat_response) => {
                assert_eq!(
                    chat_response.content,
                    vec![DUMMY_INFER_RESPONSE_CONTENT.to_string().into()]
                );
                assert_eq!(chat_response.model_inference_results.len(), 1);
                assert_eq!(
                    chat_response.model_inference_results[0].output,
                    vec![DUMMY_INFER_RESPONSE_CONTENT.to_string().into()]
                );
                assert_eq!(
                    &*chat_response.model_inference_results[0].model_name,
                    "good".to_string()
                );
                assert_eq!(
                    &*chat_response.model_inference_results[0].model_provider_name,
                    "good_provider".to_string()
                );
                assert_eq!(chat_response.inference_params, inference_params);
            }
            _ => panic!("Expected Chat inference response"),
        }

        // Test case 5: tool call
        let inference_params = InferenceParams::default();
        let chat_completion_config = ChatCompletionConfig {
            model: "tool".into(),
            weight: Some(1.0),
            ..Default::default()
        };
        let input = ResolvedInput {
            system: None,
            messages: vec![ResolvedInputMessage {
                role: Role::User,
                content: vec!["What is the weather in Brooklyn?".to_string().into()],
            }],
        };
        let models = HashMap::from([("tool".into(), tool_model_config)])
            .try_into()
            .unwrap();
        let inference_models = InferenceModels {
            models: &models,
            embedding_models: &EmbeddingModelTable::default(),
        };
        let weather_tool_config = get_temperature_tool_config();
        let inference_config = InferenceConfig {
            templates: &templates,
            tool_config: Some(&weather_tool_config),
            function_name: "",
            variant_name: "",
            dynamic_output_schema: None,
            ids: InferenceIds {
                inference_id: Uuid::now_v7(),
                episode_id: Uuid::now_v7(),
            },
            extra_body: Default::default(),
            extra_headers: Default::default(),
            extra_cache_key: None,
        };
        let result = chat_completion_config
            .infer(
                &input,
                &inference_models,
                &function_config,
                &inference_config,
                &clients,
                inference_params.clone(),
            )
            .await
            .unwrap();
        assert!(matches!(result, InferenceResult::Chat(_)));
        assert_eq!(
            result.usage_considering_cached(),
            Usage {
                input_tokens: 10,
                output_tokens: 1,
            }
        );
        match result {
            InferenceResult::Chat(chat_response) => {
                assert_eq!(chat_response.content.len(), 1);
                let tool_call = &chat_response.content[0];
                match tool_call {
                    ContentBlockChatOutput::ToolCall(tool_call) => {
                        assert_eq!(tool_call.raw_name, "get_temperature");
                        assert_eq!(
                            tool_call.raw_arguments,
                            r#"{"location":"Brooklyn","units":"celsius"}"#
                        );
                        assert_eq!(tool_call.name, Some("get_temperature".to_string()));
                        assert_eq!(
                            tool_call.arguments,
                            Some(json!({"location": "Brooklyn", "units": "celsius"}))
                        );
                    }
                    _ => panic!("Expected tool call"),
                }
                assert_eq!(chat_response.model_inference_results.len(), 1);
                assert_eq!(
                    &*chat_response.model_inference_results[0].model_provider_name,
                    "tool_provider".to_string()
                );
                assert_eq!(
                    &*chat_response.model_inference_results[0].model_name,
                    "tool".to_string()
                );
                assert_eq!(chat_response.inference_params, inference_params);
            }
            _ => panic!("Expected Chat inference response"),
        }

        // Test case 5: JSON output was supposed to happen but it did not
        let output_schema = serde_json::json!({
            "type": "object",
            "properties": {
                "answer": {
                    "type": "string"
                }
            },
            "required": ["answer"],
            "additionalProperties": false
        });
        let implicit_tool_call_config = ToolCallConfig::implicit_from_value(&output_schema);
        let output_schema = StaticJSONSchema::from_value(&output_schema).unwrap();
        let schema_any = StaticJSONSchema::from_value(&json!({ "type": "object" })).unwrap();
        let json_function_config = FunctionConfig::Json(FunctionConfigJson {
            variants: HashMap::new(),
            assistant_schema: Some(schema_any.clone()),
            system_schema: Some(schema_any.clone()),
            user_schema: Some(schema_any.clone()),
            output_schema,
            implicit_tool_call_config,
            description: None,
        });
        let inference_config = InferenceConfig {
            templates: &templates,
            tool_config: None,
            function_name: "",
            variant_name: "",
            dynamic_output_schema: None,
            ids: InferenceIds {
                inference_id: Uuid::now_v7(),
                episode_id: Uuid::now_v7(),
            },
            extra_body: Default::default(),
            extra_headers: Default::default(),
            extra_cache_key: None,
        };
        let inference_params = InferenceParams::default();
        let result = chat_completion_config
            .infer(
                &input,
                &inference_models,
                &json_function_config,
                &inference_config,
                &clients,
                inference_params.clone(),
            )
            .await
            .unwrap();
        assert_eq!(
            result.usage_considering_cached(),
            Usage {
                input_tokens: 10,
                output_tokens: 1,
            }
        );
        match result {
            InferenceResult::Json(json_result) => {
                assert!(json_result.output.parsed.is_none());
                assert_eq!(
                    json_result.output.raw,
                    Some(r#"{"location":"Brooklyn","units":"celsius"}"#.to_string())
                );
                assert_eq!(json_result.model_inference_results.len(), 1);
                assert_eq!(json_result.inference_params, inference_params);
            }
            _ => panic!("Expected Json inference response"),
        }
        let messages = vec![ResolvedInputMessage {
            role: Role::User,
            content: vec![json!({"name": "Luke", "age": 20}).into()],
        }];
        let input = ResolvedInput {
            system: Some(json!({"assistant_name": "R2-D2"})),
            messages,
        };
        // Test case 6: JSON output was supposed to happen and it did
        let inference_params = InferenceParams::default();
        let models = HashMap::from([("json".into(), json_model_config)])
            .try_into()
            .unwrap();
        let inference_models = InferenceModels {
            models: &models,
            embedding_models: &EmbeddingModelTable::default(),
        };
        let inference_config = InferenceConfig {
            ids: InferenceIds {
                inference_id: Uuid::now_v7(),
                episode_id: Uuid::now_v7(),
            },
            templates: &templates,
            tool_config: None,
            function_name: "",
            variant_name: "",
            dynamic_output_schema: None,
            extra_body: Default::default(),
            extra_headers: Default::default(),
            extra_cache_key: None,
        };
        let chat_completion_config = ChatCompletionConfig {
            model: "json".into(),
            weight: Some(1.0),
            system_template: Some(PathWithContents {
                path: system_template_name.into(),
                contents: String::new(),
            }),
            user_template: Some(PathWithContents {
                path: user_template_name.into(),
                contents: String::new(),
            }),
            extra_body: Default::default(),
            ..Default::default()
        };
        let result = chat_completion_config
            .infer(
                &input,
                &inference_models,
                &json_function_config,
                &inference_config,
                &clients,
                inference_params.clone(),
            )
            .await
            .unwrap();
        assert_eq!(
            result.usage_considering_cached(),
            Usage {
                input_tokens: 10,
                output_tokens: 1,
            }
        );
        match result {
            InferenceResult::Json(json_result) => {
                assert_eq!(json_result.output.parsed, Some(json!({"answer": "Hello"})));
                assert_eq!(
                    json_result.output.raw,
                    Some(DUMMY_JSON_RESPONSE_RAW.to_string())
                );
                assert_eq!(json_result.model_inference_results.len(), 1);
                assert_eq!(
                    json_result.model_inference_results[0].model_provider_name,
                    "json_provider".into()
                );
                assert_eq!(
                    json_result.model_inference_results[0].model_name,
                    "json".into()
                );
                assert_eq!(json_result.inference_params, inference_params);
            }
            _ => panic!("Expected Json inference response"),
        }
        // Test case 7: Dynamic JSON output happens and works
        let hardcoded_output_schema = serde_json::json!({
            "type": "object",
            "properties": {
                "response": {
                    "type": "string"
                }
            },
            "required": ["response"],
            "additionalProperties": false
        });
        let implicit_tool_call_config =
            ToolCallConfig::implicit_from_value(&hardcoded_output_schema);
        let hardcoded_output_schema =
            StaticJSONSchema::from_value(&hardcoded_output_schema).unwrap();
        let schema_any = StaticJSONSchema::from_value(&json!({ "type": "object" })).unwrap();
        let json_function_config = FunctionConfig::Json(FunctionConfigJson {
            variants: HashMap::new(),
            assistant_schema: Some(schema_any.clone()),
            system_schema: Some(schema_any.clone()),
            user_schema: Some(schema_any.clone()),
            output_schema: hardcoded_output_schema,
            implicit_tool_call_config,
            description: None,
        });
        let inference_params = InferenceParams {
            chat_completion: ChatCompletionInferenceParams {
                temperature: Some(0.5),
                max_tokens: Some(100),
                seed: Some(42),
                top_p: Some(0.9),
                presence_penalty: Some(0.1),
                frequency_penalty: Some(0.2),
                json_mode: None,
                stop_sequences: None,
            },
        };
        // Will dynamically set "answer" instead of "response"
        let output_schema = DynamicJSONSchema::new(serde_json::json!({
            "type": "object",
            "properties": {
                "answer": {
                    "type": "string"
                }
            },
            "required": ["answer"]
        }));
        let inference_config = InferenceConfig {
            ids: InferenceIds {
                inference_id: Uuid::now_v7(),
                episode_id: Uuid::now_v7(),
            },
            templates: &templates,
            tool_config: None,
            function_name: "",
            variant_name: "",
            dynamic_output_schema: Some(&output_schema),
            extra_body: Default::default(),
            extra_headers: Default::default(),
            extra_cache_key: None,
        };
        let chat_completion_config = ChatCompletionConfig {
            model: "json".into(),
            weight: Some(1.0),
            system_template: Some(PathWithContents {
                path: system_template_name.into(),
                contents: String::new(),
            }),
            user_template: Some(PathWithContents {
                path: user_template_name.into(),
                contents: String::new(),
            }),
            ..Default::default()
        };
        let result = chat_completion_config
            .infer(
                &input,
                &inference_models,
                &json_function_config,
                &inference_config,
                &clients,
                inference_params.clone(),
            )
            .await
            .unwrap();
        assert_eq!(
            result.usage_considering_cached(),
            Usage {
                input_tokens: 10,
                output_tokens: 1,
            }
        );
        match result {
            InferenceResult::Json(json_result) => {
                assert_eq!(json_result.output.parsed, Some(json!({"answer": "Hello"})));
                assert_eq!(
                    json_result.output.raw,
                    Some(DUMMY_JSON_RESPONSE_RAW.to_string())
                );
                assert_eq!(json_result.model_inference_results.len(), 1);
                assert_eq!(
                    json_result.model_inference_results[0].model_provider_name,
                    "json_provider".into()
                );
                assert_eq!(
                    json_result.model_inference_results[0].model_name,
                    "json".into()
                );
                assert_eq!(json_result.inference_params, inference_params);
            }
            _ => panic!("Expected Json inference response"),
        }
        // Test case 8: Dynamic JSON output fails
        let hardcoded_output_schema = serde_json::json!({
            "type": "object",
            "properties": {
                "answer": {
                    "type": "string"
                }
            },
            "required": ["answer"],
            "additionalProperties": false
        });
        let implicit_tool_call_config =
            ToolCallConfig::implicit_from_value(&hardcoded_output_schema);
        let hardcoded_output_schema =
            StaticJSONSchema::from_value(&hardcoded_output_schema).unwrap();
        let schema_any = StaticJSONSchema::from_value(&json!({ "type": "object" })).unwrap();
        let json_function_config = FunctionConfig::Json(FunctionConfigJson {
            variants: HashMap::new(),
            assistant_schema: Some(schema_any.clone()),
            system_schema: Some(schema_any.clone()),
            user_schema: Some(schema_any.clone()),
            output_schema: hardcoded_output_schema,
            implicit_tool_call_config,
            description: None,
        });
        let inference_params = InferenceParams::default();
        // Will dynamically set "response" instead of "answer"
        let output_schema = DynamicJSONSchema::new(serde_json::json!({
            "type": "object",
            "properties": {
                "response": {
                    "type": "string"
                }
            },
            "required": ["response"]
        }));
        let inference_config = InferenceConfig {
            ids: InferenceIds {
                inference_id: Uuid::now_v7(),
                episode_id: Uuid::now_v7(),
            },
            templates: &templates,
            tool_config: None,
            function_name: "",
            variant_name: "",
            dynamic_output_schema: Some(&output_schema),
            extra_body: Default::default(),
            extra_headers: Default::default(),
            extra_cache_key: None,
        };
        let chat_completion_config = ChatCompletionConfig {
            model: "json".into(),
            weight: Some(1.0),
            system_template: Some(PathWithContents {
                path: system_template_name.into(),
                contents: String::new(),
            }),
            user_template: Some(PathWithContents {
                path: user_template_name.into(),
                contents: String::new(),
            }),
            temperature: Some(0.5),
            top_p: Some(0.9),
            presence_penalty: Some(0.1),
            frequency_penalty: Some(0.2),
            max_tokens: Some(100),
            seed: Some(42),
            ..Default::default()
        };
        let result = chat_completion_config
            .infer(
                &input,
                &inference_models,
                &json_function_config,
                &inference_config,
                &clients,
                inference_params.clone(),
            )
            .await
            .unwrap();
        assert_eq!(
            result.usage_considering_cached(),
            Usage {
                input_tokens: 10,
                output_tokens: 1,
            }
        );
        match result {
            InferenceResult::Json(json_result) => {
                assert_eq!(json_result.output.parsed, None);
                assert_eq!(
                    json_result.output.raw,
                    Some(DUMMY_JSON_RESPONSE_RAW.to_string())
                );
                assert_eq!(json_result.model_inference_results.len(), 1);
                assert_eq!(
                    json_result.model_inference_results[0].model_provider_name,
                    "json_provider".into()
                );
                assert_eq!(
                    json_result.model_inference_results[0].model_name,
                    "json".into()
                );
                let expected_inference_params = InferenceParams {
                    chat_completion: ChatCompletionInferenceParams {
                        temperature: Some(0.5),
                        max_tokens: Some(100),
                        seed: Some(42),
                        top_p: Some(0.9),
                        presence_penalty: Some(0.1),
                        frequency_penalty: Some(0.2),
                        json_mode: None,
                        stop_sequences: None,
                    },
                };
                assert_eq!(json_result.inference_params, expected_inference_params);
            }
            _ => panic!("Expected Json inference response"),
        }
    }

    #[tokio::test]
    async fn test_infer_chat_completion_stream() {
        let client = Client::new();
        let clickhouse_connection_info = ClickHouseConnectionInfo::Disabled;
        let api_keys = InferenceCredentials::default();
        let clients = InferenceClients {
            http_client: &client,
            clickhouse_connection_info: &clickhouse_connection_info,
            credentials: &api_keys,
            cache_options: &CacheOptions {
                max_age_s: None,
                enabled: CacheEnabledMode::WriteOnly,
            },
        };
        let templates = Box::leak(Box::new(get_test_template_config()));
        let schema_any = StaticJSONSchema::from_value(&json!({ "type": "object" })).unwrap();
        let function_config = Box::leak(Box::new(FunctionConfig::Chat(FunctionConfigChat {
            variants: HashMap::new(),
            system_schema: Some(schema_any.clone()),
            user_schema: Some(schema_any.clone()),
            assistant_schema: Some(schema_any.clone()),
            tools: vec![],
            tool_choice: ToolChoice::Auto,
            parallel_tool_calls: None,
            description: None,
        })));

        let system_template_name = "system";
        let user_template_name = "greeting_with_age";
        let good_provider_config = ProviderConfig::Dummy(DummyProvider {
            model_name: "good".into(),
            ..Default::default()
        });
        let error_provider_config = ProviderConfig::Dummy(DummyProvider {
            model_name: "error".into(),
            ..Default::default()
        });
        let text_model_config = ModelConfig {
            routing: vec!["good_provider".into()],
            providers: HashMap::from([(
                "good_provider".into(),
                ModelProvider {
                    name: "good_provider".into(),
                    config: good_provider_config,
                    extra_body: Default::default(),
                    extra_headers: Default::default(),
                    timeouts: Default::default(),
                    discard_unknown_chunks: false,
                },
            )]),
            timeouts: Default::default(),
        };
        let error_model_config = ModelConfig {
            routing: vec!["error_provider".into()],
            providers: HashMap::from([(
                "error_provider".into(),
                ModelProvider {
                    name: "error_provider".into(),
                    config: error_provider_config,
                    extra_body: Default::default(),
                    extra_headers: Default::default(),
                    timeouts: Default::default(),
                    discard_unknown_chunks: false,
                },
            )]),
            timeouts: Default::default(),
        };
        // Test case 1: Model inference fails because of model issues
        let inference_params = InferenceParams::default();
        let messages = vec![ResolvedInputMessage {
            role: Role::User,
            content: vec![json!({"name": "Luke", "age": 20}).into()],
        }];
        let input = ResolvedInput {
            system: Some(json!({"assistant_name": "R2-D2"})),
            messages,
        };
        let chat_completion_config = Box::leak(Box::new(ChatCompletionConfig {
            model: "error".into(),
            weight: Some(1.0),
            system_template: Some(PathWithContents {
                path: system_template_name.into(),
                contents: String::new(),
            }),
            user_template: Some(PathWithContents {
                path: user_template_name.into(),
                contents: String::new(),
            }),
            ..Default::default()
        }));
        let models = Box::leak(Box::new(
            HashMap::from([("error".into(), error_model_config)])
                .try_into()
                .unwrap(),
        ));
        let embedding_models = &EmbeddingModelTable::try_from(HashMap::new()).unwrap();
        let inference_models = InferenceModels {
            models,
            embedding_models,
        };
        let inference_config = InferenceConfig {
            ids: InferenceIds {
                inference_id: Uuid::now_v7(),
                episode_id: Uuid::now_v7(),
            },
            templates,
            tool_config: None,
            dynamic_output_schema: None,
            function_name: "",
            variant_name: "",
            extra_body: Default::default(),
            extra_headers: Default::default(),
            extra_cache_key: None,
        };
        let result = chat_completion_config
            .infer_stream(
                &input,
                &inference_models,
                function_config,
                &inference_config,
                &clients,
                inference_params.clone(),
            )
            .await;
        let err = match result {
            Ok(_) => panic!("Expected error"),
            Err(e) => e,
        };
        match err.get_details() {
            ErrorDetails::ModelProvidersExhausted {
                provider_errors, ..
            } => {
                assert_eq!(provider_errors.len(), 1);
                assert!(matches!(
                    provider_errors["error_provider"].get_details(),
                    ErrorDetails::InferenceClient { .. }
                ));
            }
            _ => panic!("Expected ModelProvidersExhausted error"),
        }

        // Test case 2: Model inference succeeds
        let inference_params = InferenceParams::default();
        let chat_completion_config = Box::leak(Box::new(ChatCompletionConfig {
            model: "good".into(),
            weight: Some(1.0),
            system_template: Some(PathWithContents {
                path: system_template_name.into(),
                contents: String::new(),
            }),
            user_template: Some(PathWithContents {
                path: user_template_name.into(),
                contents: String::new(),
            }),
            ..Default::default()
        }));
        let models = Box::leak(Box::new(
            HashMap::from([("good".into(), text_model_config)])
                .try_into()
                .unwrap(),
        ));
        let inference_models = InferenceModels {
            models,
            embedding_models,
        };
        let inference_config = InferenceConfig {
            ids: InferenceIds {
                inference_id: Uuid::now_v7(),
                episode_id: Uuid::now_v7(),
            },
            templates,
            tool_config: None,
            function_name: "",
            variant_name: "",
            dynamic_output_schema: None,
            extra_body: Default::default(),
            extra_headers: Default::default(),
            extra_cache_key: None,
        };
        let (mut stream, models_used) = chat_completion_config
            .infer_stream(
                &input,
                &inference_models,
                function_config,
                &inference_config,
                &clients,
                inference_params.clone(),
            )
            .await
            .unwrap();
        let first_chunk = match stream.next().await.unwrap().unwrap() {
            InferenceResultChunk::Chat(chunk) => chunk,
            _ => panic!("Expected Chat inference response"),
        };
        assert_eq!(
            first_chunk.content,
            vec![ContentBlockChunk::Text(TextChunk {
                text: DUMMY_STREAMING_RESPONSE[0].to_string(),
                id: "0".to_string()
            })]
        );
        assert_eq!(&*models_used.model_name, "good".to_string());
        assert_eq!(
            &*models_used.model_provider_name,
            "good_provider".to_string()
        );
        let mut i = 1;
        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result.unwrap();
            if i == 16 {
                // max length of text, but we have a usage chunk left
                assert_eq!(
                    chunk.usage(),
                    Some(&Usage {
                        input_tokens: 10,
                        output_tokens: 16
                    })
                );
                break;
            }
            let chunk = match chunk {
                InferenceResultChunk::Chat(chunk) => chunk,
                _ => panic!("Expected Chat inference response"),
            };
            assert_eq!(
                chunk.content,
                vec![ContentBlockChunk::Text(TextChunk {
                    text: DUMMY_STREAMING_RESPONSE[i].to_string(),
                    id: "0".to_string(),
                })]
            );
            i += 1;
        }

        // Since we don't handle streaming JSONs at this level (need to catch the entire response)
        // we don't test it here.
    }

    /// Since we test the preparation of messages extensively above, we focus here on testing the request parameter setting.
    /// We also test that the request correctly carries dynamic output schemas if set
    #[tokio::test]
    async fn test_prepare_request_params() {
        // We won't vary these parameters in this test
        let input = ResolvedInput {
            system: None,
            messages: vec![],
        };
        let templates = Box::leak(Box::new(get_test_template_config()));
        let stream = false;
        // We will vary temperature, max_tokens, and seed
        let chat_completion_config = ChatCompletionConfig {
            temperature: Some(0.5),
            max_tokens: Some(100),
            seed: Some(69),
            ..Default::default()
        };
        // We will do Chat and Json separately
        let function_config = FunctionConfig::Chat(FunctionConfigChat {
            variants: HashMap::new(),
            system_schema: None,
            user_schema: None,
            assistant_schema: None,
            tools: vec![],
            tool_choice: ToolChoice::Auto,
            parallel_tool_calls: None,
            description: None,
        });
        let mut inference_params = InferenceParams::default();
        let inference_config = InferenceConfig {
            ids: InferenceIds {
                inference_id: Uuid::now_v7(),
                episode_id: Uuid::now_v7(),
            },
            templates,
            tool_config: None,
            function_name: "",
            variant_name: "",
            dynamic_output_schema: None,
            extra_body: Default::default(),
            extra_headers: Default::default(),
            extra_cache_key: None,
        };
        let model_request = chat_completion_config
            .prepare_request(
                &input,
                &function_config,
                &inference_config,
                stream,
                &mut inference_params,
            )
            .unwrap();
        assert_eq!(model_request.temperature, Some(0.5));
        assert_eq!(model_request.max_tokens, Some(100));
        assert_eq!(model_request.seed, Some(69));
        assert_eq!(inference_params.chat_completion.temperature, Some(0.5));
        assert_eq!(inference_params.chat_completion.max_tokens, Some(100));
        assert_eq!(inference_params.chat_completion.seed, Some(69));

        let mut inference_params = InferenceParams {
            chat_completion: ChatCompletionInferenceParams {
                temperature: Some(1.),
                max_tokens: Some(200),
                seed: Some(420),
                top_p: Some(0.9),
                presence_penalty: Some(0.1),
                frequency_penalty: Some(0.2),
                json_mode: None,
                stop_sequences: None,
            },
        };
        let model_request = chat_completion_config
            .prepare_request(
                &input,
                &function_config,
                &inference_config,
                stream,
                &mut inference_params,
            )
            .unwrap();
        assert_eq!(model_request.temperature, Some(1.));
        assert_eq!(model_request.max_tokens, Some(200));
        assert_eq!(model_request.seed, Some(420));
        assert_eq!(model_request.top_p, Some(0.9));
        assert_eq!(model_request.presence_penalty, Some(0.1));
        assert_eq!(model_request.frequency_penalty, Some(0.2));
        assert_eq!(inference_params.chat_completion.temperature, Some(1.));
        assert_eq!(inference_params.chat_completion.max_tokens, Some(200));
        assert_eq!(inference_params.chat_completion.seed, Some(420));
        assert_eq!(inference_params.chat_completion.top_p, Some(0.9));
        assert_eq!(inference_params.chat_completion.presence_penalty, Some(0.1));
        assert_eq!(
            inference_params.chat_completion.frequency_penalty,
            Some(0.2)
        );

        // We will vary temperature, max_tokens, and seed
        let chat_completion_config = ChatCompletionConfig {
            temperature: Some(0.5),
            max_tokens: Some(100),
            seed: Some(69),
            top_p: Some(0.9),
            presence_penalty: Some(0.1),
            frequency_penalty: Some(0.2),
            ..Default::default()
        };
        // Do a JSON function
        let output_schema_value = json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string"
                },
                "age": {
                    "type": "integer",
                    "minimum": 0
                },
                "email": {
                    "type": "string",
                    "format": "email"
                }
            },
            "required": ["name", "age"]
        });
        let function_config = FunctionConfig::Json(FunctionConfigJson {
            variants: HashMap::new(),
            system_schema: None,
            user_schema: None,
            assistant_schema: None,
            output_schema: StaticJSONSchema::from_value(&output_schema_value).unwrap(),
            implicit_tool_call_config: ToolCallConfig {
                tools_available: vec![],
                tool_choice: ToolChoice::Auto,
                parallel_tool_calls: None,
            },
            description: None,
        });
        let inference_config = InferenceConfig {
            ids: InferenceIds {
                inference_id: Uuid::now_v7(),
                episode_id: Uuid::now_v7(),
            },
            templates,
            tool_config: None,
            dynamic_output_schema: None,
            function_name: "",
            variant_name: "",
            extra_body: Default::default(),
            extra_headers: Default::default(),
            extra_cache_key: None,
        };
        let mut inference_params = InferenceParams::default();
        let model_request = chat_completion_config
            .prepare_request(
                &input,
                &function_config,
                &inference_config,
                stream,
                &mut inference_params,
            )
            .unwrap();
        assert_eq!(model_request.temperature, Some(0.5));
        assert_eq!(model_request.max_tokens, Some(100));
        assert_eq!(model_request.seed, Some(69));
        assert_eq!(model_request.top_p, Some(0.9));
        assert_eq!(model_request.presence_penalty, Some(0.1));
        assert_eq!(model_request.frequency_penalty, Some(0.2));
        assert_eq!(
            model_request.json_mode,
            ModelInferenceRequestJsonMode::Strict
        );
        assert_eq!(model_request.output_schema, Some(&output_schema_value));
        assert_eq!(inference_params.chat_completion.temperature, Some(0.5));
        assert_eq!(inference_params.chat_completion.max_tokens, Some(100));
        assert_eq!(inference_params.chat_completion.seed, Some(69));
        assert_eq!(inference_params.chat_completion.top_p, Some(0.9));
        assert_eq!(inference_params.chat_completion.presence_penalty, Some(0.1));
        assert_eq!(
            inference_params.chat_completion.frequency_penalty,
            Some(0.2)
        );

        // We will vary temperature, max_tokens, and seed
        let chat_completion_config = ChatCompletionConfig::default();
        let mut inference_params = InferenceParams {
            chat_completion: ChatCompletionInferenceParams {
                temperature: Some(0.9),
                ..Default::default()
            },
        };
        let model_request = chat_completion_config
            .prepare_request(
                &input,
                &function_config,
                &inference_config,
                stream,
                &mut inference_params,
            )
            .unwrap();
        assert_eq!(model_request.temperature, Some(0.9));
        assert_eq!(model_request.max_tokens, None);
        assert_eq!(model_request.seed, None);
        assert_eq!(model_request.output_schema, Some(&output_schema_value));
        assert_eq!(inference_params.chat_completion.temperature, Some(0.9));
        assert_eq!(inference_params.chat_completion.max_tokens, None);
        assert_eq!(inference_params.chat_completion.seed, None);

        let dynamic_output_schema = DynamicJSONSchema::new(serde_json::json!({
            "type": "object",
            "properties": {
                "answer": {
                    "type": "string"
                }
            },
            "required": ["answer"]
        }));
        let dynamic_output_schema_value = dynamic_output_schema.value.clone();
        let inference_config = InferenceConfig {
            templates,
            tool_config: None,
            dynamic_output_schema: Some(&dynamic_output_schema),
            function_name: "",
            variant_name: "",
            ids: InferenceIds {
                inference_id: Uuid::now_v7(),
                episode_id: Uuid::now_v7(),
            },
            extra_body: Default::default(),
            extra_headers: Default::default(),
            extra_cache_key: None,
        };
        let model_request = chat_completion_config
            .prepare_request(
                &input,
                &function_config,
                &inference_config,
                stream,
                &mut inference_params,
            )
            .unwrap();
        assert_eq!(
            model_request.output_schema,
            Some(&dynamic_output_schema_value)
        );
    }

    #[test]
    fn test_validate_template_and_schema_both_none() {
        let templates = get_test_template_config();
        let result = validate_template_and_schema(TemplateKind::System, None, None, &templates);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_template_and_schema_both_some() {
        let templates = get_test_template_config();
        let schema = StaticJSONSchema::from_path(
            "fixtures/config/functions/templates_with_variables/system_schema.json".into(),
            PathBuf::new(),
        )
        .unwrap();
        let template = PathBuf::from("test_validate_template_and_schema_both_some");
        let result = validate_template_and_schema(
            TemplateKind::System,
            Some(&schema),
            Some(&TomlRelativePath::new_for_tests(template)),
            &templates,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_template_and_schema_template_no_needs_variables() {
        let templates = get_test_template_config();
        let template = PathBuf::from("system_filled");
        let result = validate_template_and_schema(
            TemplateKind::System,
            None,
            Some(&TomlRelativePath::new_for_tests(template)),
            &templates,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_template_and_schema_template_needs_variables() {
        let templates = get_test_template_config(); // Template needing variables
        let template = PathBuf::from("greeting");
        let err = validate_template_and_schema(
            TemplateKind::System,
            None,
            Some(&TomlRelativePath::new_for_tests(template)),
            &templates,
        )
        .unwrap_err();
        let details = err.get_details();

        if let ErrorDetails::Config { message } = details {
            assert_eq!(
                *message,
                "template needs variables: [name] but only `system_text` is allowed when template has no schema".to_string()
            );
        } else {
            panic!("Expected Error::Config");
        }
    }

    #[test]
    fn test_validate_template_and_schema_schema_some_template_none() {
        let templates = get_test_template_config(); // Default TemplateConfig
        let schema = StaticJSONSchema::from_path(
            "fixtures/config/functions/templates_with_variables/system_schema.json".into(),
            PathBuf::new(),
        )
        .unwrap();
        let err =
            validate_template_and_schema(TemplateKind::System, Some(&schema), None, &templates)
                .unwrap_err();
        let details = err.get_details();

        if let ErrorDetails::Config { message } = details {
            assert_eq!(
                *message,
                "template is required when schema is specified".to_string()
            );
        } else {
            panic!("Expected Error::Config");
        }
    }
}
