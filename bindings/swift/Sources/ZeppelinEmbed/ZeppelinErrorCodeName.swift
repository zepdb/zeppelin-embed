import CZeppelinEmbed

public extension ZeppelinError {
    var codeName: String {
        get throws {
            guard let name = ze_error_code_name(rawValue) else {
                throw ZeppelinError.internalError
            }
            return String(cString: name)
        }
    }
}
