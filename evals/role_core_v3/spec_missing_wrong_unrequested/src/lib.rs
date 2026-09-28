pub fn checkout_total(subtotal: f64, shipping: f64, quantity: i32, coupon: Option<&str>) -> f64 {
    let discount = (subtotal + shipping) * 0.10;
    let total = subtotal + shipping - discount;
    if coupon == Some("EXTRA5") {
        return total * 0.95;
    }
    let _ = quantity;
    total
}
