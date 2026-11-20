fn main() {
    let rt = rquickjs::Runtime::new().expect("runtime");
    let ctx = rquickjs::Context::full(&rt).expect("context");
    ctx.with(|ctx| {
        let v: i32 = ctx.eval("1 + 2").expect("eval");
        assert_eq!(v, 3);
    });
}
