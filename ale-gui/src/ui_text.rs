pub fn event(key: &str, english: bool) -> String {
    let (zh, en) = match key {
        "Connected" => ("会话建立成功", "Session connected"),
        "Disconnected" => ("连接已断开", "Session disconnected"),
        "Ready" => ("等待手机指令", "Waiting for a command"),
        "Paused" => (
            "已暂停，当前请求已取消",
            "Paused; current request cancelled",
        ),
        "Transcribing" => ("正在识别语音", "Transcribing speech"),
        "CapturingState" => ("正在获取屏幕状态", "Capturing screen state"),
        "Summarizing" => ("正在分析屏幕", "Analyzing the screen"),
        "Planning" => ("正在生成操作计划", "Planning actions"),
        "Grounding" => ("正在定位可交互元素", "Locating interactive elements"),
        "AwaitingDecision" => ("等待确认", "Waiting for confirmation"),
        "Executing" => ("正在执行操作", "Executing actions"),
        "Verifying" => ("正在验证执行结果", "Verifying the result"),
        "PreviewReady" => ("操作预览已就绪", "Action preview ready"),
        "Completed" => ("执行完成", "Completed"),
        "Failed" => ("请求失败，查看详情", "Request failed; see details"),
        "Cancelled" => ("请求已取消", "Request cancelled"),
        "ConfirmPlan" => ("操作计划 · 待确认", "Action plan · confirmation required"),
        "LowRiskPlan" => ("低风险操作 · 待确认", "Low risk · confirmation required"),
        "MediumRiskPlan" => ("中风险操作 · 待确认", "Medium risk · confirmation required"),
        "HighRiskPlan" => ("高风险操作 · 待确认", "High risk · confirmation required"),
        "UseRemoteModel" => (
            "使用云端模型 · 待确认",
            "Cloud processing · confirmation required",
        ),
        "UploadFullScreenshot" => (
            "上传完整截图 · 待确认",
            "Full screenshot upload · confirmation required",
        ),
        "RiskChanged" => (
            "操作风险变化 · 待确认",
            "Risk changed · confirmation required",
        ),
        _ => return key.to_string(),
    };
    if english { en } else { zh }.to_string()
}
