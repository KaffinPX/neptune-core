use convert_case::Casing;
use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{ItemEnum, parse_macro_input};

pub fn json_router_derive(input: TokenStream) -> TokenStream {
    let enum_item = parse_macro_input!(input as ItemEnum);
    let enum_name = &enum_item.ident;

    let mut match_arms = vec![];

    for variant in &enum_item.variants {
        let variant_name = &variant.ident;
        let variant_str = variant_name.to_string();

        // Parse #[namespace(Namespaces::Chain)]
        let namespace = variant
            .attrs
            .iter()
            .find(|attr| attr.path().is_ident("namespace"))
            .and_then(|attr| attr.parse_args::<syn::ExprPath>().ok())
            .expect("Each variant must have #[namespace(...)]");

        // Build method name: e.g. "chain_getHeight"
        let namespace_str = namespace
            .path
            .segments
            .last()
            .expect("Invalid namespace")
            .ident
            .to_string()
            .to_case(convert_case::Case::Snake);
        let method_suffix = variant_str.to_case(convert_case::Case::Camel);
        let method_name = format!("{}_{}", namespace_str, method_suffix);

        // Build corresponding Request Response variant + _call function
        let req_type = format_ident!("{}Request", variant_name);
        let res_type = format_ident!("{}Response", variant_name);
        let call_fn = format_ident!("{}_call", variant_str.to_case(convert_case::Case::Snake));

        match_arms.push(quote! {
            #method_name => {
                let params = serde_json::from_value::<#req_type>(params).map_err(|_| RpcError::InvalidParams)?;
                let result: #res_type = api.#call_fn(params).await;
                Ok(serde_json::to_value(result).map_err(|_| RpcError::InternalError)?)
            }
        });
    }

    let expanded = quote! {
        impl #enum_name {
            pub async fn dispatch(
                api: &std::sync::Arc<dyn RpcApi>,
                method: &str,
                params: serde_json::Value,
            ) -> RpcResult<serde_json::Value> {
                match method {
                    #(#match_arms,)*
                    _ => Err(RpcError::MethodNotFound),
                }
            }
        }
    };

    expanded.into()
}
