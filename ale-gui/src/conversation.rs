pub(crate) fn automation_tools() -> Vec<serde_json::Value> {
    vec![serde_json::json!({
        "type": "function",
        "function": {
            "name": "execute_action_plan",
            "description": "Create a desktop automation plan for the visible screen. The app will show the plan to the user for confirmation before executing it.",
            "parameters": {
                "type": "object",
                "properties": {
                    "explanation": {
                        "type": "string",
                        "description": "Short natural-language explanation of what will be done."
                    },
                    "risk_level": {
                        "type": "string",
                        "enum": ["low", "medium", "high"]
                    },
                    "requires_confirmation": {
                        "type": "boolean",
                        "description": "Whether the user must confirm before execution. Use true for any action that changes data, opens apps, types, clicks, or uses files."
                    },
                    "actions": {
                        "type": "array",
                        "items": {
                            "oneOf": [
                                {
                                    "type": "object",
                                    "properties": {
                                        "type": { "type": "string", "enum": ["click"] },
                                        "x": { "type": "number" },
                                        "y": { "type": "number" },
                                        "button": { "type": "string", "enum": ["left", "right", "middle"] }
                                    },
                                    "required": ["type", "x", "y", "button"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "type": { "type": "string", "enum": ["double_click"] },
                                        "x": { "type": "number" },
                                        "y": { "type": "number" }
                                    },
                                    "required": ["type", "x", "y"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "type": { "type": "string", "enum": ["mouse_move"] },
                                        "x": { "type": "number" },
                                        "y": { "type": "number" }
                                    },
                                    "required": ["type", "x", "y"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "type": { "type": "string", "enum": ["scroll"] },
                                        "x": { "type": "number" },
                                        "y": { "type": "number" },
                                        "delta_x": { "type": "number" },
                                        "delta_y": { "type": "number" }
                                    },
                                    "required": ["type", "x", "y", "delta_x", "delta_y"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "type": { "type": "string", "enum": ["type"] },
                                        "text": { "type": "string" }
                                    },
                                    "required": ["type", "text"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "type": { "type": "string", "enum": ["key"] },
                                        "key": { "type": "string" },
                                        "modifiers": { "type": "array", "items": { "type": "string" } }
                                    },
                                    "required": ["type", "key", "modifiers"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "type": { "type": "string", "enum": ["wait"] },
                                        "ms": { "type": "integer" }
                                    },
                                    "required": ["type", "ms"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "type": { "type": "string", "enum": ["open_app"] },
                                        "name": { "type": "string" }
                                    },
                                    "required": ["type", "name"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "type": { "type": "string", "enum": ["open_url"] },
                                        "url": { "type": "string" }
                                    },
                                    "required": ["type", "url"]
                                }
                            ]
                        }
                    }
                },
                "required": ["explanation", "risk_level", "requires_confirmation", "actions"]
            }
        }
    })]
}
