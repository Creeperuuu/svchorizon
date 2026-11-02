fn svchorizon_native_mods(
    mods: &mut Vec<crate::game::native_hooks::NativeMod>,
) {
    crate::game::svchorizon::on_mods(mods);
}
