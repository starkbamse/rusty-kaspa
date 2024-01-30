use crate::imports::*;

#[wasm_bindgen(typescript_custom_section)]
const IScriptPublicKey:&'static str= r#"
interface IScriptPublicKey {
    version: number
    scriptPublicKey: string
}
"#;

#[wasm_bindgen(typescript_custom_section)]
const IOutpoint:&'static str= r#"
interface IOutpoint {
    transactionId: string
    index: number
}
"#;
#[wasm_bindgen(typescript_custom_section)]
const IUtxoEntry:&'static str= r#"
interface IUtxoEntry {
    amount: bigint
    scriptPublicKey: IScriptPublicKey | undefined
    blockDaaScore: bigint
    isCoinbase: boolean
}
"#;

#[wasm_bindgen(typescript_custom_section)]
const IGetUtxosByAddressesResponse:&'static str= r#"
interface IUtxosByAddressesEntry {
    address: string
    outpoint: IOutpoint | undefined
    utxoEntry: IUtxoEntry | undefined
}
"#;


#[wasm_bindgen(typescript_custom_section)]
const IGetUtxosByAddressesResponse:&'static str= r#"
interface IGetUtxosByAddressesResponse {
    entries: IUtxosByAddressesEntry[]
    error: { string } | undefined
}
"#;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(typescript_type="Address[] | string[]")]
    pub type GetUtxosByAddressesRequest;

    #[wasm_bindgen(typescript_type="IGetUtxosByAddressesResponse")]
    pub type IGetUtxosByAddressesResponse;

}

impl IGetUtxosByAddressesResponse {
    pub fn new(value: JsValue) -> Self {
        IGetUtxosByAddressesResponse { obj: value }
    }
}
