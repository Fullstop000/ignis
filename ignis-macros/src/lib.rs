use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::quote;
use syn::{parse_macro_input, FnArg, ItemFn, LitStr, Token, Pat, Type, parse::{Parse, ParseStream}};

struct ToolArgs {
    name: Option<String>,
    description: String,
}

impl Parse for ToolArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut name = None;
        let mut description = None;

        while !input.is_empty() {
            let ident: syn::Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            let val: LitStr = input.parse()?;

            if ident == "name" {
                name = Some(val.value());
            } else if ident == "description" {
                description = Some(val.value());
            } else {
                return Err(syn::Error::new_spanned(
                    ident,
                    "Unknown attribute argument. Allowed: 'name', 'description'"
                ));
            }

            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }

        let description = description.ok_or_else(|| {
            syn::Error::new(input.span(), "Missing required argument 'description'")
        })?;

        Ok(ToolArgs { name, description })
    }
}

fn get_inner_type_from_option(ty: &Type) -> Option<&Type> {
    if let Type::Path(type_path) = ty {
        if let Some(segment) = type_path.path.segments.last() {
            if segment.ident == "Option" {
                if let syn::PathArguments::AngleBracketed(args) = &segment.arguments {
                    if let Some(syn::GenericArgument::Type(inner_ty)) = args.args.first() {
                        return Some(inner_ty);
                    }
                }
            }
        }
    }
    None
}

fn map_type_to_json_schema(ty: &Type) -> &'static str {
    let check_ty = get_inner_type_from_option(ty).unwrap_or(ty);
    if let Type::Path(type_path) = check_ty {
        if let Some(segment) = type_path.path.segments.last() {
            let ident_str = segment.ident.to_string();
            match ident_str.as_str() {
                "String" | "str" | "char" => "string",
                "i8" | "i16" | "i32" | "i64" | "isize" |
                "u8" | "u16" | "u32" | "u64" | "usize" => "integer",
                "f32" | "f64" => "number",
                "bool" => "boolean",
                _ => "string", // Fallback to string
            }
        } else {
            "string"
        }
    } else {
        "string"
    }
}

#[proc_macro_attribute]
pub fn tool(args: TokenStream, input: TokenStream) -> TokenStream {
    let tool_args = parse_macro_input!(args as ToolArgs);
    let func = parse_macro_input!(input as ItemFn);

    let func_name = &func.sig.ident;
    let tool_name = tool_args.name.unwrap_or_else(|| func_name.to_string());
    let description = tool_args.description;

    let vis = &func.vis;
    let attrs = &func.attrs;

    // Struct name to generate, e.g. GetWeather -> GetWeatherTool
    let camel_case_name = tool_name
        .split('_')
        .map(|s| {
            let mut c = s.chars();
            match c.next() {
                None => String::new(),
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            }
        })
        .collect::<String>();
    let struct_ident = syn::Ident::new(&format!("{}Tool", camel_case_name), Span::call_site());

    // Parse function parameters
    let mut arg_names = Vec::new();
    let mut arg_types = Vec::new();
    let mut required_names = Vec::new();
    let mut properties_code = Vec::new();

    for input_arg in &func.sig.inputs {
        if let FnArg::Typed(pat_type) = input_arg {
            let arg_name = if let Pat::Ident(pat_ident) = &*pat_type.pat {
                pat_ident.ident.to_string()
            } else {
                continue;
            };

            let ty = &pat_type.ty;
            let is_optional = get_inner_type_from_option(ty).is_some();
            let json_type = map_type_to_json_schema(ty);

            arg_names.push(syn::Ident::new(&arg_name, Span::call_site()));
            arg_types.push(ty.clone());

            if !is_optional {
                required_names.push(arg_name.clone());
            }

            properties_code.push(quote! {
                properties.insert(#arg_name.to_string(), serde_json::json!({
                    "type": #json_type
                }));
            });
        }
    }

    let required_code = quote! {
        let required: Vec<String> = vec![#(#required_names.to_string()),*];
    };

    let args_struct_ident = syn::Ident::new(&format!("{}Args", camel_case_name), Span::call_site());

    // Generate output code
    let expanded = quote! {
        #(#attrs)*
        #vis #func

        #[derive(serde::Deserialize)]
        struct #args_struct_ident {
            #(#arg_names: #arg_types),*
        }

        pub struct #struct_ident;

        #[async_trait::async_trait]
        impl ignis::AgentTool for #struct_ident {
            fn name(&self) -> &str {
                #tool_name
            }

            fn description(&self) -> &str {
                #description
            }

            fn parameters(&self) -> serde_json::Value {
                let mut properties = serde_json::Map::new();
                #(#properties_code)*
                #required_code
                serde_json::json!({
                    "type": "object",
                    "properties": properties,
                    "required": required
                })
            }

            async fn call(
                &self,
                args: serde_json::Value,
            ) -> ignis::ToolResult {
                let parsed: #args_struct_ident = match serde_json::from_value(args) {
                    Ok(v) => v,
                    Err(e) => return ignis::ToolResult::error(format!("Failed to parse arguments: {}", e)),
                };
                let res = #func_name(#(parsed.#arg_names),*).await;
                ignis::IntoToolResult::into_tool_result(res)
            }
        }
    };

    TokenStream::from(expanded)
}
