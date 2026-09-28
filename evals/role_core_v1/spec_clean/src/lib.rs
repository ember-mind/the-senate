pub fn checkout_total(subtotal: f64, shipping: f64, quantity: i32) -> Result<f64, &'static str> {
    if quantity < 0 {
        return Err("quantity cannot be negative");
    }
    let discount = subtotal * 0.10;
    Ok(subtotal + shipping - discount)
}
