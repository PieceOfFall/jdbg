package dev.jdbg.sidecar;

final class RpcException extends Exception {
    final String code;

    RpcException(String code, String message) {
        super(message);
        this.code = code;
    }

    RpcException(String code, String message, Throwable cause) {
        super(message, cause);
        this.code = code;
    }
}
