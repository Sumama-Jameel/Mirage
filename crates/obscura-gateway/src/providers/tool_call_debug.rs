#[test]
fn dbg_inline() {
    let text = "get_weather(location='Paris', units={'temp': 'c'}, days=[1, 2])";
    eprintln!("direct inline: {:?}", crate::providers::tool_call::parse_inline_function_call(text));
    eprintln!("gemini: {:?}", crate::providers::tool_call::parse_gemini_tool_calls(text));
}
