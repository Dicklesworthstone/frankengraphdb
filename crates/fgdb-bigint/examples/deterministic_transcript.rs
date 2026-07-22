use std::error::Error;

use fgdb_bigint::{ArithmeticError, BigInt, LimbLimit, Sign};

fn print_value(label: &str, value: &BigInt) {
    let sign = match value.sign() {
        Sign::Negative => "negative",
        Sign::Zero => "zero",
        Sign::Positive => "positive",
    };
    print!("{label}: sign={sign} limbs_le=[");
    for (index, limb) in value.magnitude_limbs_le().iter().enumerate() {
        if index != 0 {
            print!(",");
        }
        print!("{limb:016x}");
    }
    println!("]");
}

fn main() -> Result<(), Box<dyn Error>> {
    let limit = LimbLimit::new(8);
    let one = BigInt::from_u64(1);
    let max_limb = BigInt::from_u64(u64::MAX);

    let carry = max_limb.checked_add(&one, limit)?;
    let negative = carry.checked_neg(limit)?;
    let square = max_limb.checked_mul(&max_limb, limit)?;
    let restored = negative.checked_add(&carry, LimbLimit::new(0))?;

    print_value("carry", &carry);
    print_value("negative", &negative);
    print_value("square", &square);
    print_value("restored", &restored);

    match max_limb.checked_add(&one, LimbLimit::new(1)) {
        Err(ArithmeticError::LimbLimitExceeded {
            operation,
            required_limbs,
            limit,
        }) => println!(
            "bounded_failure: operation={operation} required_limbs={required_limbs} limit={limit}"
        ),
        Err(other) => return Err(other.into()),
        Ok(_) => {
            return Err(std::io::Error::other(
                "one-limb limit unexpectedly admitted a two-limb sum",
            )
            .into());
        }
    }

    Ok(())
}
