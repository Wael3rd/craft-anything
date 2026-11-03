// Craft Anything: turn off Ghidra's slowest whole-program analysis passes before a target is
// analysed. Each runs the decompiler over every function in the program, which can take hours on
// a big game; decompiling a chosen function later works that function out by itself.
// With `entry=0x…` (raw machine code, which has no header saying where it starts), mark that
// address as the program's entry and make a function there, so the analysis has somewhere to
// begin and finds what it calls.
//@category CraftAnything

import ghidra.app.script.GhidraScript;
import ghidra.program.model.address.Address;
import java.util.Map;

public class CraftAnythingFastAnalysis extends GhidraScript {
    private static final String[] SLOW = {
        "Decompiler Switch Analysis",
        "Decompiler Parameter ID",
        "Aggressive Instruction Finder",
        "Embedded Media",
    };

    @Override
    public void run() throws Exception {
        Map<String, String> options = getCurrentAnalysisOptionsAndValues(currentProgram);
        for (String name : SLOW) {
            if (options.containsKey(name)) {
                setAnalysisOption(currentProgram, name, "false");
                println("Craft Anything: skipping " + name);
            }
        }
        for (String arg : getScriptArgs()) {
            if (arg.startsWith("entry=")) {
                startAnalysisAt(arg.substring("entry=".length()));
            }
        }
    }

    /** Mark `hex` as the entry point and make a function there (Ghidra's own FUN_ name). */
    private void startAnalysisAt(String hex) {
        String digits = hex.startsWith("0x") || hex.startsWith("0X") ? hex.substring(2) : hex;
        long offset;
        try {
            offset = Long.parseUnsignedLong(digits, 16);
        } catch (NumberFormatException e) {
            println("Craft Anything: entry point not understood: " + hex);
            return;
        }
        Address at = toAddr(offset);
        if (at == null || !currentProgram.getMemory().contains(at)) {
            println("Craft Anything: entry point " + hex + " is outside the program");
            return;
        }
        addEntryPoint(at);
        if (getFunctionAt(at) == null) {
            disassemble(at);
            createFunction(at, null);
        }
        println("Craft Anything: analysis starts at " + at);
    }
}
