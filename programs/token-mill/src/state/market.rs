use std::cmp::min;

use anchor_lang::prelude::*;

use crate::{
    constant::*,
    errors::TokenMillError,
    manager::swap_manager::SwapAmountType,
    math::{div, get_delta_base_in, get_delta_base_out, mul_div, Rounding},
};

pub const MARKET_PDA_SEED: &str = "market";

#[zero_copy]
#[derive(Debug, InitSpace)]
pub struct MarketFees {
    /// staking_fee_share + creator_fee_share + protocol_fee_share = 100%
    pub staking_fee_share: u16,
    pub creator_fee_share: u16,
    _space: u32,

    pub pending_staking_fees: u64,
    pub pending_creator_fees: u64,
}

#[account(zero_copy)]
#[derive(Debug, InitSpace)]
pub struct Market {
    pub config: Pubkey,
    pub creator: Pubkey,

    pub base_token_mint: Pubkey,
    pub quote_token_mint: Pubkey,

    pub base_reserve: u64,

    pub bid_prices: [u64; PRICES_LENGTH],
    pub ask_prices: [u64; PRICES_LENGTH],

    pub width_scaled: u64,
    pub total_supply: u64,

    pub fees: MarketFees,

    pub quote_token_decimals: u8,
    pub bump: u8,

    pub _space: [u8; 6],
}

impl MarketFees {
    pub fn distribute_fee(
        &mut self,
        swap_fee: u64,
        referral_fee_share: Option<u16>,
    ) -> Result<(u64, u64, u64, u64)> {
        let creator_fee = u128::from(swap_fee)
            .checked_mul(u128::from(self.creator_fee_share))
            .and_then(|v| v.checked_div(MAX_BPS as u128))
            .and_then(|v| u64::try_from(v).ok())
            .ok_or(TokenMillError::MathError)?;

        let staking_fee = u128::from(swap_fee)
            .checked_mul(u128::from(self.staking_fee_share))
            .and_then(|v| v.checked_div(MAX_BPS as u128))
            .and_then(|v| u64::try_from(v).ok())
            .ok_or(TokenMillError::MathError)?;

        let remaining_fee = swap_fee
            .checked_sub(creator_fee)
            .and_then(|v| v.checked_sub(staking_fee))
            .ok_or(TokenMillError::MathError)?;

        let referral_fee = if let Some(referral_fee_share) = referral_fee_share {
            u128::from(remaining_fee)
                .checked_mul(u128::from(referral_fee_share))
                .and_then(|v| v.checked_div(MAX_BPS as u128))
                .and_then(|v| u64::try_from(v).ok())
                .ok_or(TokenMillError::MathError)?
        } else {
            0
        };

        let protocol_fee = remaining_fee
            .checked_sub(referral_fee)
            .ok_or(TokenMillError::MathError)?;

        self.pending_creator_fees = self
            .pending_creator_fees
            .checked_add(creator_fee)
            .ok_or(TokenMillError::MathError)?;
        self.pending_staking_fees = self
            .pending_staking_fees
            .checked_add(staking_fee)
            .ok_or(TokenMillError::MathError)?;

        Ok((creator_fee, staking_fee, protocol_fee, referral_fee))
    }
}

impl Market {
    #[allow(clippy::too_many_arguments)]
    pub fn initialize(
        &mut self,
        bump: u8,
        config: Pubkey,
        creator: Pubkey,
        base_token_mint: Pubkey,
        quote_token_mint: Pubkey,
        quote_token_decimals: u8,
        total_supply: u64,
        creator_fee_share: u16,
        staking_fee_share: u16,
    ) -> Result<()> {
        if total_supply > MAX_TOTAL_SUPPLY
            || total_supply
                .checked_div(INTERVAL_NUMBER)
                .ok_or(TokenMillError::MathError)?
                < BASE_PRECISION
            || total_supply
                .checked_div(INTERVAL_NUMBER)
                .ok_or(TokenMillError::MathError)?
                .checked_mul(INTERVAL_NUMBER)
                .ok_or(TokenMillError::MathError)?
                != total_supply
        {
            return Err(TokenMillError::InvalidTotalSupply.into());
        }

        self.bump = bump;
        self.config = config;
        self.creator = creator;
        self.base_token_mint = base_token_mint;
        self.quote_token_mint = quote_token_mint;
        self.quote_token_decimals = quote_token_decimals;
        self.total_supply = total_supply;
        self.base_reserve = total_supply;
        self.width_scaled = u128::from(
            total_supply
                .checked_div(INTERVAL_NUMBER)
                .ok_or(TokenMillError::MathError)?,
        )
        .checked_mul(SCALE)
        .and_then(|v| v.checked_div(u128::from(BASE_PRECISION)))
        .and_then(|v| u64::try_from(v).ok())
        .ok_or(TokenMillError::MathError)?;

        self.fees.creator_fee_share = creator_fee_share;
        self.fees.staking_fee_share = staking_fee_share;
        Ok(())
    }

    pub fn check_and_set_prices(
        &mut self,
        bid_prices: [u64; PRICES_LENGTH],
        ask_prices: [u64; PRICES_LENGTH],
    ) -> Result<()> {
        if self.are_prices_set() {
            return Err(TokenMillError::PricesAlreadySet.into());
        }

        for i in 0..PRICES_LENGTH {
            let bid_price = bid_prices[i];
            let ask_price = ask_prices[i];

            if bid_price > ask_price {
                return Err(TokenMillError::BidAskMismatch.into());
            }

            if i > 0 && (ask_price <= ask_prices[i - 1] || bid_price <= bid_prices[i - 1]) {
                return Err(TokenMillError::DecreasingPrices.into());
            }
        }

        if ask_prices[INTERVAL_NUMBER as usize] > MAX_PRICE {
            return Err(TokenMillError::PriceTooHigh.into());
        }

        self.bid_prices = bid_prices;
        self.ask_prices = ask_prices;

        Ok(())
    }

    pub fn are_prices_set(&self) -> bool {
        self.ask_prices[INTERVAL_NUMBER as usize] != 0
    }

    pub fn circulating_supply(&self) -> u64 {
        self.total_supply
            .checked_sub(self.base_reserve)
            .unwrap_or(0)
    }

    pub fn get_quote_amount(
        &self,
        base_amount: u64,
        swap_amount_type: SwapAmountType,
    ) -> Result<(u64, u64)> {
        let circulating_supply = self.circulating_supply();

        let (supply, rounding) = match swap_amount_type {
            SwapAmountType::ExactInput => (
                circulating_supply
                    .checked_sub(base_amount)
                    .ok_or(TokenMillError::MathError)?,
                Rounding::Down,
            ),
            SwapAmountType::ExactOutput => (circulating_supply, Rounding::Up),
        };

        self.get_quote_amount_with_parameters(supply, base_amount, swap_amount_type, rounding)
    }

    pub fn get_quote_amount_with_parameters(
        &self,
        supply: u64,
        base_amount: u64,
        swap_amount_type: SwapAmountType,
        rounding: Rounding,
    ) -> Result<(u64, u64)> {
        let price_curve = match swap_amount_type {
            SwapAmountType::ExactInput => &self.bid_prices,
            SwapAmountType::ExactOutput => &self.ask_prices,
        };

        let normalized_supply = u128::from(supply)
            .checked_mul(SCALE)
            .and_then(|v| v.checked_div(u128::from(BASE_PRECISION)))
            .ok_or(TokenMillError::MathError)?;

        let mut normalized_base_amount_left = u128::from(base_amount)
            .checked_mul(SCALE)
            .and_then(|v| v.checked_div(u128::from(BASE_PRECISION)))
            .ok_or(TokenMillError::MathError)?;

        let mut normalized_quote_amount: u128 = 0;

        let mut i = usize::try_from(
            normalized_supply
                .checked_div(u128::from(self.width_scaled))
                .ok_or(TokenMillError::MathError)?,
        )
        .map_err(|_| TokenMillError::MathError)?;
        let mut interval_supply_already_used = normalized_supply
            .checked_rem(u128::from(self.width_scaled))
            .ok_or(TokenMillError::MathError)?;

        let mut price_0 = *price_curve.get(i).ok_or(TokenMillError::MathError)?;
        i += 1;

        while normalized_base_amount_left > 0 && i < PRICES_LENGTH {
            let price_1 = price_curve[i];

            let delta_base = min(
                normalized_base_amount_left,
                u128::from(self.width_scaled)
                    .checked_sub(interval_supply_already_used)
                    .ok_or(TokenMillError::MathError)?,
            );

            let delta_quote = mul_div(
                delta_base,
                u128::from(
                    price_1
                        .checked_sub(price_0)
                        .ok_or(TokenMillError::MathError)?,
                )
                .checked_mul(
                    delta_base
                        .checked_add(2 * interval_supply_already_used)
                        .ok_or(TokenMillError::MathError)?,
                )
                .and_then(|v| {
                    v.checked_add(2 * u128::from(price_0) * u128::from(self.width_scaled))
                })
                .ok_or(TokenMillError::MathError)?,
                2 * SCALE * u128::from(self.width_scaled),
                rounding,
            )
            .ok_or(TokenMillError::MathError)?;

            normalized_base_amount_left = normalized_base_amount_left
                .checked_sub(delta_base)
                .ok_or(TokenMillError::MathError)?;
            normalized_quote_amount = normalized_quote_amount
                .checked_add(delta_quote)
                .ok_or(TokenMillError::MathError)?;

            interval_supply_already_used = 0;
            price_0 = price_1;

            i += 1;
        }

        let base_amount_swapped = base_amount
            .checked_sub(div(
                normalized_base_amount_left
                    .checked_mul(u128::from(BASE_PRECISION))
                    .ok_or(TokenMillError::MathError)?,
                SCALE,
                rounding,
            )?)
            .ok_or(TokenMillError::MathError)?;

        let quote_amount_swapped = div(
            normalized_quote_amount
                .checked_mul(u128::pow(10, u32::from(self.quote_token_decimals)))
                .ok_or(TokenMillError::MathError)?,
            SCALE,
            rounding,
        )?;

        Ok((base_amount_swapped, quote_amount_swapped))
    }

    pub fn get_base_amount_in(&self, quote_amount: u64) -> Result<(u64, u64)> {
        let price_curve = &self.bid_prices;
        let circulating_supply = self.circulating_supply();

        let normalized_supply = u128::from(circulating_supply)
            .checked_mul(SCALE)
            .and_then(|v| v.checked_div(u128::from(BASE_PRECISION)))
            .ok_or(TokenMillError::MathError)?;

        let quote_precision = u128::pow(10, u32::from(self.quote_token_decimals));
        let mut normalized_quote_amount_left = u128::from(quote_amount)
            .checked_mul(SCALE)
            .and_then(|v| v.checked_div(quote_precision))
            .ok_or(TokenMillError::MathError)?;
        let mut normalized_base_amount: u128 = 0;

        let mut i = usize::try_from(
            normalized_supply
                .checked_div(u128::from(self.width_scaled))
                .ok_or(TokenMillError::MathError)?,
        )
        .map_err(|_| TokenMillError::MathError)?;
        let mut interval_supply_available = normalized_supply
            .checked_rem(u128::from(self.width_scaled))
            .ok_or(TokenMillError::MathError)?;

        if interval_supply_available == 0 {
            interval_supply_available = u128::from(self.width_scaled);
        } else {
            i += 1;
        }

        let mut price_1 = price_curve[i];

        while normalized_quote_amount_left > 0 && i > 0 {
            let price_0 = price_curve[i - 1];

            let (delta_base, delta_quote) = get_delta_base_in(
                price_0.into(),
                price_1.into(),
                self.width_scaled.into(),
                interval_supply_available,
                normalized_quote_amount_left,
            )?;

            normalized_base_amount = normalized_base_amount
                .checked_add(delta_base)
                .ok_or(TokenMillError::MathError)?;
            normalized_quote_amount_left = normalized_quote_amount_left
                .checked_sub(delta_quote)
                .ok_or(TokenMillError::MathError)?;

            interval_supply_available = u128::from(self.width_scaled);
            price_1 = price_0;

            i -= 1;
        }

        let base_amount_swapped = div(
            normalized_base_amount
                .checked_mul(u128::from(BASE_PRECISION))
                .ok_or(TokenMillError::MathError)?,
            SCALE,
            Rounding::Up,
        )?;

        let quote_amount_swapped = quote_amount
            .checked_sub(div(
                normalized_quote_amount_left
                    .checked_mul(quote_precision)
                    .ok_or(TokenMillError::MathError)?,
                SCALE,
                Rounding::Up,
            )?)
            .ok_or(TokenMillError::MathError)?;

        Ok((base_amount_swapped, quote_amount_swapped))
    }

    pub fn get_base_amount_out(&self, quote_amount: u64) -> Result<(u64, u64)> {
        let price_curve = &self.ask_prices;
        let circulating_supply = self.circulating_supply();

        let normalized_supply = u128::from(circulating_supply)
            .checked_mul(SCALE)
            .and_then(|v| v.checked_div(u128::from(BASE_PRECISION)))
            .ok_or(TokenMillError::MathError)?;

        let quote_precision = u128::pow(10, u32::from(self.quote_token_decimals));
        let mut normalized_quote_amount_left = u128::from(quote_amount)
            .checked_mul(SCALE)
            .and_then(|v| v.checked_div(quote_precision))
            .ok_or(TokenMillError::MathError)?;
        let mut normalized_base_amount: u128 = 0;

        let mut i = usize::try_from(
            normalized_supply
                .checked_div(u128::from(self.width_scaled))
                .ok_or(TokenMillError::MathError)?,
        )
        .map_err(|_| TokenMillError::MathError)?;
        let mut interval_supply_already_used = normalized_supply
            .checked_rem(u128::from(self.width_scaled))
            .ok_or(TokenMillError::MathError)?;

        let mut price_0 = price_curve[i];

        while normalized_quote_amount_left > 0 && i < PRICES_LENGTH - 1 {
            let price_1 = price_curve[i + 1];

            let (delta_base, delta_quote) = get_delta_base_out(
                price_0.into(),
                price_1.into(),
                self.width_scaled.into(),
                interval_supply_already_used,
                normalized_quote_amount_left,
            )?;

            normalized_base_amount = normalized_base_amount
                .checked_add(delta_base)
                .ok_or(TokenMillError::MathError)?;
            normalized_quote_amount_left = normalized_quote_amount_left
                .checked_sub(delta_quote)
                .ok_or(TokenMillError::MathError)?;

            interval_supply_already_used = 0;
            price_0 = price_1;

            i += 1;
        }

        let base_amount_swapped = div(
            normalized_base_amount
                .checked_mul(u128::from(BASE_PRECISION))
                .ok_or(TokenMillError::MathError)?,
            SCALE,
            Rounding::Down,
        )?;

        let quote_amount_swapped = quote_amount
            .checked_sub(div(
                normalized_quote_amount_left
                    .checked_mul(quote_precision)
                    .ok_or(TokenMillError::MathError)?,
                SCALE,
                Rounding::Down,
            )?)
            .ok_or(TokenMillError::MathError)?;

        Ok((base_amount_swapped, quote_amount_swapped))
    }
}

#[cfg(test)]
mod tests {
    use anchor_lang::Space;

    use crate::state::Market;

    #[test]
    fn size() {
        let size = Market::INIT_SPACE + 8;

        println!("Size of Market: {}", size);

        assert!(size < 10_240);
    }
}
