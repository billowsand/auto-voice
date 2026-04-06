use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    temperature: f32,
    stream: bool,
    /// Qwen3 专用：关闭 thinking 模式，避免输出 <think>...</think> 推理过程
    #[serde(skip_serializing_if = "Option::is_none")]
    chat_template_kwargs: Option<serde_json::Value>,
}

#[derive(Serialize, Deserialize)]
struct Message {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: Message,
}

/// 用于文件转写（会议录音）的校对 prompt
const MEETING_PROMPT: &str = "\
你是一个专业的会议记录校对助手。\
对语音识别结果进行后处理，要求：\
1. 修正明显的同音错别字（如「的地得」、专业术语）\
2. 补全合理的标点符号\
3. 不改变原意，不增删实质内容\
4. 保持原有分段结构\
5. 直接返回校正后的文本，不要解释";

/// 用于实时语音输入的润色 prompt（专为小模型设计，few-shot 约束）
const VOICE_INPUT_SYSTEM: &str = "\
你是一个文字格式化工具，只能对输入文字做最小化处理，严禁回复、回答、续写或扩展内容。

处理规则（仅此三条）：
1. 修正明显的同音错别字
2. 补全自然的标点符号
3. 原文有什么就输出什么，不增不减

示例：
输入：今天天气不错出去玩吧
输出：今天天气不错，出去玩吧。

示例：
输入：你好我是小明最好的学习方式就是实践
输出：你好，我是小明，最好的学习方式就是实践。

示例：
输入：Cloud编程工具很好用
输出：Cloud编程工具很好用。

如果输入是问句，也只格式化输出该问句，不回答。";

/// 会议录音文件转写校对（async，用于 transcribe 命令）
pub async fn correct_text(
    client: &Client,
    base_url: &str,
    model: &str,
    text: &str,
) -> Result<String> {
    chat_completion_async(client, base_url, model, MEETING_PROMPT, text, 0.1).await
}

/// 实时语音输入润色（blocking，在独立线程中调用，不阻塞麦克风采集）
pub fn polish_voice_blocking(base_url: &str, model: &str, text: &str) -> Result<String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;

    let url = format!("{}/v1/chat/completions", base_url.trim_end_matches('/'));
    // 在 user message 里再次声明任务，帮助小模型理解（不能只依赖 system prompt）
    let user_msg = format!("只做格式化，不要回答，直接输出结果：\n{}", text);
    let request = ChatRequest {
        model: model.to_string(),
        messages: vec![
            Message {
                role: "system".to_string(),
                content: VOICE_INPUT_SYSTEM.to_string(),
            },
            Message {
                role: "user".to_string(),
                content: user_msg,
            },
        ],
        temperature: 0.1,
        stream: false,
        chat_template_kwargs: Some(serde_json::json!({"enable_thinking": false})),
    };

    let resp = client
        .post(&url)
        .json(&request)
        .send()?
        .error_for_status()?
        .json::<ChatResponse>()?;

    let result = resp
        .choices
        .into_iter()
        .next()
        .map(|c| strip_think_tags(&c.message.content))
        .unwrap_or_else(|| text.to_string());

    // 防御：若 LLM 输出远长于原文，说明在"回答"而非润色，回退原文
    if result.chars().count() > text.chars().count() * 2 + 20 {
        tracing::warn!(
            "LLM output suspiciously long ({} vs {}), falling back to raw ASR",
            result.chars().count(),
            text.chars().count()
        );
        return Ok(text.to_string());
    }

    Ok(result)
}

async fn chat_completion_async(
    client: &Client,
    base_url: &str,
    model: &str,
    system: &str,
    user: &str,
    temperature: f32,
) -> Result<String> {
    let url = format!("{}/v1/chat/completions", base_url.trim_end_matches('/'));
    let request = ChatRequest {
        model: model.to_string(),
        messages: vec![
            Message {
                role: "system".to_string(),
                content: system.to_string(),
            },
            Message {
                role: "user".to_string(),
                content: user.to_string(),
            },
        ],
        temperature,
        stream: false,
        chat_template_kwargs: Some(serde_json::json!({"enable_thinking": false})),
    };

    let resp = client
        .post(&url)
        .json(&request)
        .send()
        .await?
        .error_for_status()?
        .json::<ChatResponse>()
        .await?;

    Ok(resp
        .choices
        .into_iter()
        .next()
        .map(|c| strip_think_tags(&c.message.content))
        .unwrap_or_else(|| user.to_string()))
}

/// 剥离 Qwen3 thinking 模式输出的 <think>...</think> 块。
/// 即使 enable_thinking=false 不生效（如 LMStudio 版本不支持该参数），也能兜底。
fn strip_think_tags(text: &str) -> String {
    let mut result = text.trim().to_string();
    // 去掉 <think>...</think> 及其内容（可能跨行）
    while let (Some(start), Some(end)) = (result.find("<think>"), result.find("</think>")) {
        if start <= end {
            result = format!("{}{}", &result[..start], &result[end + 8..])
                .trim()
                .to_string();
        } else {
            break;
        }
    }
    result
}
