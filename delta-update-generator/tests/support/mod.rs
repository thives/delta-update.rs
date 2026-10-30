use delta_update_generator::DevicePreference;

pub fn device() -> DevicePreference {
    return match std::env::var("OTA_TEST_DEVICE").as_deref() {
        Ok("auto") => DevicePreference::Auto,
        _ => DevicePreference::Software,
    };
}
