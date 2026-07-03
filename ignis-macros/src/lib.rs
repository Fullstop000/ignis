use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::quote;
use syn::{
    parse::{Parse, ParseStream},
    parse_macro_input, FnArg, ItemFn, LitStr, Pat, Token, Type,
};

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
                    "Unknown attribute argument. Allowed: 'name', 'description'",
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

/// Maps a Rust parameter type to a JSON-Schema expression.
/// Returns a `TokenStream2` that evaluates to a `serde_json::Value`.
fn map_type_to_json_schema(ty: &Type) -> syn::Result<TokenStream2> {
    // Option<T> is represented by the schema of T; the field is marked optional
    // via the `required` array, not via the schema type.
    if let Some(inner) = get_inner_type_from_option(ty) {
        return map_type_to_json_schema(inner);
    }

    match ty {
        Type::Array(type_array) => {
            let inner = map_type_to_json_schema(&type_array.elem)?;
            Ok(quote! { serde_json::json!({ "type": "array", "items": #inner }) })
        }
        Type::Slice(type_slice) => {
            let inner = map_type_to_json_schema(&type_slice.elem)?;
            Ok(quote! { serde_json::json!({ "type": "array", "items": #inner }) })
        }
        Type::Reference(type_ref) => {
            // &[T] is a slice reference, which cannot be deserialized from a
            // `serde_json::Value` (it needs a borrowed lifetime). Point users to
            // Vec<T> instead.
            if type_ref.mutability.is_none() {
                if let Type::Slice(_) = &*type_ref.elem {
                    return Err(syn::Error::new_spanned(
                        ty,
                        "slice references (&[T]) are not supported by #[tool]; use Vec<T> instead",
                    ));
                }
            }
            Err(syn::Error::new_spanned(
                ty,
                "references are not supported by #[tool]; use owned types like String or Vec<T>",
            ))
        }
        Type::Path(type_path) => {
            if let Some(segment) = type_path.path.segments.last() {
                let ident_str = segment.ident.to_string();
                match ident_str.as_str() {
                    "Vec" => {
                        if let syn::PathArguments::AngleBracketed(args) = &segment.arguments {
                            if let Some(syn::GenericArgument::Type(inner_ty)) = args.args.first() {
                                let inner = map_type_to_json_schema(inner_ty)?;
                                return Ok(quote! {
                                    serde_json::json!({ "type": "array", "items": #inner })
                                });
                            }
                        }
                        Err(syn::Error::new_spanned(ty, "Vec requires a type argument"))
                    }
                    "HashMap" | "BTreeMap" => {
                        Ok(quote! { serde_json::json!({ "type": "object" }) })
                    }
                    "String" | "str" | "char" => {
                        Ok(quote! { serde_json::json!({ "type": "string" }) })
                    }
                    "i8" | "i16" | "i32" | "i64" | "isize" | "u8" | "u16" | "u32" | "u64"
                    | "usize" => Ok(quote! { serde_json::json!({ "type": "integer" }) }),
                    "f32" | "f64" => Ok(quote! { serde_json::json!({ "type": "number" }) }),
                    "bool" => Ok(quote! { serde_json::json!({ "type": "boolean" }) }),
                    // Other path types (structs, enums, type aliases) are treated as objects.
                    _ => Ok(quote! { serde_json::json!({ "type": "object" }) }),
                }
            } else {
                Ok(quote! { serde_json::json!({ "type": "object" }) })
            }
        }
        _ => Err(syn::Error::new_spanned(
            ty,
            "unsupported parameter type for #[tool]",
        )),
    }
}

#[proc_macro_attribute]
/// The `#[tool]` attribute turns an async function into an `ignis::AgentTool`.
///
/// Supported parameter types map to the corresponding JSON Schema type:
/// - scalars (`String`, integers, floats, `bool`) → `string`, `integer`, `number`, `boolean`
/// - `Vec<T>` and fixed-size arrays `[T; N]` → `array` with `items` schemas
/// - maps (`HashMap`, `BTreeMap`) and structs/enums → `object`
/// - `Option<T>` → schema of `T`, and the field is omitted from `required`
///
/// Unsupported types (references, tuples, slices, etc.) are rejected at compile
/// time rather than silently mapped to `"string"`.
///
/// ```compile_fail
/// use ignis_macros::tool;
///
/// #[tool(name = "bad", description = "bad")]
/// async fn bad_tool(_x: (String, String)) -> Result<String, String> { Ok(String::new()) }
/// ```
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
            let schema_expr = match map_type_to_json_schema(ty) {
                Ok(expr) => expr,
                Err(e) => return e.to_compile_error().into(),
            };

            arg_names.push(syn::Ident::new(&arg_name, Span::call_site()));
            arg_types.push(ty.clone());

            if !is_optional {
                required_names.push(arg_name.clone());
            }

            properties_code.push(quote! {
                properties.insert(#arg_name.to_string(), #schema_expr);
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
