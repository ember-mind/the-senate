pub fn checkout_total(subtotal: f64, shipping: f64, quantity: i32, coupon: Option<&str>) -> f64 {
    let discount = (subtotal + shipping) * 0.10;
    let mut total = subtotal + shipping - discount;
    if quantity == 0 {
        total = 0.0;
    }
    if coupon == Some("EXTRA5") {
        total *= 0.95;
    }
    total
}
