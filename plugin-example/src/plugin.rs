use sapio::contract::*;
use sapio::*;
use sapio_wasm_plugin::*;
use schemars::*;
use serde::*;

use std::convert::TryInto;

use std::os::raw::c_char;

#[derive(JsonSchema, Serialize, Deserialize, Clone)]
pub struct Payment {
    pub amount: bitcoin::util::amount::CoinAmount,
    /// # Address
    /// The Address to send to
    pub address: bitcoin::Address,
}
#[derive(JsonSchema, Serialize, Deserialize)]
pub struct TreePay {
    pub participants: Vec<Payment>,
    pub radix: usize,
}

impl TreePay {
    then! {expand |s, ctx| {
        let mut builder = ctx.template();
        if s.participants.len() > s.radix {

            for c in s.participants.chunks(s.participants.len()/s.radix) {
                let mut amt =  bitcoin::util::amount::Amount::from_sat(0);
                for Payment{amount, ..}  in c {
                    amt += amount.clone().try_into()?;
                }
                builder = builder.add_output(amt, &TreePay {participants: c.to_vec(), radix: s.radix}, None)?;
            }
        } else {
            for Payment{amount, address} in s.participants.iter() {
                builder = builder.add_output((*amount).try_into()?, &Compiled::from_address(address.clone(), None), None)?;
            }
        }
        builder.into()
    }}
}
impl Contract for TreePay {
    declare! {then, Self::expand}
    declare! {non updatable}
}
impl Plugin for TreePay {
    #[no_mangle]
    fn get_api() -> *mut c_char {
        Self::get_api_inner()
    }
    #[no_mangle]
    unsafe fn create(p: *mut i8) -> *mut i8 {
        client::create::<Self>(p)
    }
}
