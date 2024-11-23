package org.opensearch.migrations.transform.jinjava;

import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.Optional;

import com.hubspot.jinjava.interpret.Context;
import com.hubspot.jinjava.interpret.JinjavaInterpreter;
import com.hubspot.jinjava.lib.fn.ELFunctionDefinition;
import com.hubspot.jinjava.lib.fn.Functions;
import com.hubspot.jinjava.objects.collections.PyList;
import com.hubspot.jinjava.lib.fn.MacroFunction;

public class DynamicMacroFunction {

    /**
     * Called from templates through the registration in the JinjavaTransformer class
     */
    public static Object invokeMacro(String macroName, Object... args) {
        JinjavaInterpreter interpreter = JinjavaInterpreter.getCurrent();

        MacroFunction macro = getMacroFromContext(interpreter.getContext(), macroName);

        Context macroContext = new Context(interpreter.getContext());
        int argCount = Math.min(args.length, macro.getArguments().size());

        for (int i = 0; i < argCount; i++) {
            String argName = macro.getArguments().get(i);
            macroContext.put(argName, args[i]);
        }

        var argsMap = new HashMap<String,Object>();

        var paramNames = macro.getArguments();
        Map<String, Object> defaults = macro.getDefaults();

        for (int i = 0; i < paramNames.size(); i++) {
            String paramName = paramNames.get(i);
            if (i < args.length) {
                argsMap.put(paramName, args[i]);
            } else if (defaults.containsKey(paramName)) {
                argsMap.put(paramName, defaults.get(paramName));
            } else {
                throw new RuntimeException("Missing argument for macro: " + paramName);
            }
        }

        return macro.doEvaluate(argsMap,  Map.of(), List.of());
    }

    private static MacroFunction getMacroFromContext(Context context, String macroName) {
        if (context == null) {
            return null;
        }
        return context.getLocalMacro(macroName)
            .or(() -> Optional.ofNullable(context.getGlobalMacro(macroName)))
            .orElseGet(() -> getMacroFromContext(context.getParent(), macroName));
    }
}
