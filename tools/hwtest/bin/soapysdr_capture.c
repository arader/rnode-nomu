#include <SoapySDR/Device.h>
#include <SoapySDR/Formats.h>
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static void usage(const char *argv0) {
    fprintf(stderr, "Usage: %s OUT.cfile FREQ_HZ SAMPLE_RATE_HZ SECONDS [GAIN_DB]\n", argv0);
}

int main(int argc, char **argv) {
    if (argc < 5 || argc > 6) {
        usage(argv[0]);
        return 2;
    }
    const char *out_path = argv[1];
    const double freq = atof(argv[2]);
    const double rate = atof(argv[3]);
    const double seconds = atof(argv[4]);
    const double gain = argc == 6 ? atof(argv[5]) : 24.0;
    if (freq <= 0 || rate <= 0 || seconds <= 0) {
        usage(argv[0]);
        return 2;
    }

    SoapySDRKwargs args = {0};
    SoapySDRKwargs_set(&args, "driver", "rtlsdr");
    SoapySDRDevice *dev = SoapySDRDevice_make(&args);
    SoapySDRKwargs_clear(&args);
    if (!dev) {
        fprintf(stderr, "SoapySDRDevice_make failed: %s\n", SoapySDRDevice_lastError());
        return 1;
    }
    if (SoapySDRDevice_setSampleRate(dev, SOAPY_SDR_RX, 0, rate) != 0 ||
        SoapySDRDevice_setFrequency(dev, SOAPY_SDR_RX, 0, freq, NULL) != 0 ||
        SoapySDRDevice_setGain(dev, SOAPY_SDR_RX, 0, gain) != 0) {
        fprintf(stderr, "SoapySDR configure failed: %s\n", SoapySDRDevice_lastError());
        SoapySDRDevice_unmake(dev);
        return 1;
    }

    SoapySDRStream *stream = SoapySDRDevice_setupStream(dev, SOAPY_SDR_RX, SOAPY_SDR_CF32, NULL, 0, NULL);
    if (!stream) {
        fprintf(stderr, "setupStream failed: %s\n", SoapySDRDevice_lastError());
        SoapySDRDevice_unmake(dev);
        return 1;
    }
    FILE *out = fopen(out_path, "wb");
    if (!out) {
        fprintf(stderr, "open %s failed: %s\n", out_path, strerror(errno));
        SoapySDRDevice_closeStream(dev, stream);
        SoapySDRDevice_unmake(dev);
        return 1;
    }

    const size_t total = (size_t)(rate * seconds);
    const size_t block = 16384;
    float *buf = calloc(block * 2, sizeof(float));
    if (!buf) {
        fprintf(stderr, "calloc failed\n");
        fclose(out);
        SoapySDRDevice_closeStream(dev, stream);
        SoapySDRDevice_unmake(dev);
        return 1;
    }
    void *buffs[] = {buf};
    int flags = 0;
    long long time_ns = 0;
    if (SoapySDRDevice_activateStream(dev, stream, 0, 0, 0) != 0) {
        fprintf(stderr, "activateStream failed: %s\n", SoapySDRDevice_lastError());
        free(buf);
        fclose(out);
        SoapySDRDevice_closeStream(dev, stream);
        SoapySDRDevice_unmake(dev);
        return 1;
    }
    size_t got = 0;
    while (got < total) {
        int want = (int)((total - got) < block ? (total - got) : block);
        int n = SoapySDRDevice_readStream(dev, stream, buffs, want, &flags, &time_ns, 1000000);
        if (n > 0) {
            fwrite(buf, sizeof(float) * 2, (size_t)n, out);
            got += (size_t)n;
        } else if (n == SOAPY_SDR_TIMEOUT) {
            continue;
        } else {
            fprintf(stderr, "readStream failed after %zu samples: %s (%d)\n", got, SoapySDR_errToStr(n), n);
            break;
        }
    }
    SoapySDRDevice_deactivateStream(dev, stream, 0, 0);
    fprintf(stderr, "captured %zu complex samples to %s at %.0f Hz centered %.0f Hz gain %.1f dB\n", got, out_path, rate, freq, gain);
    free(buf);
    fclose(out);
    SoapySDRDevice_closeStream(dev, stream);
    SoapySDRDevice_unmake(dev);
    return got == total ? 0 : 1;
}
